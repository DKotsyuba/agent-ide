//! Real acceptance coverage for the bounded shared Unix `gopls` profile.

#[path = "support/provider.rs"]
#[allow(dead_code)]
mod provider;

use std::{
    collections::BTreeSet,
    env, fs, io,
    ops::ControlFlow,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use agent_ide::{
    execution::{
        Admission, AdmissionClass, AdmissionController, AdmissionLease, AdmissionLimits,
        DirectChildReap, LocalExecutionPolicy, OwnedChild, OwnedProtocolChild, OwnerId,
        ProviderBackendKind, ProviderLeaseAdmission, ProviderLeaseError, ProviderLeaseLimits,
        ProviderLeaseRegistry, ProviderViewLease, ValidatedExecutionRequest,
        ValidatedHostInvocation, WorkspaceAuthority,
    },
    intelligence::gopls::{GoplsProfile, SharedGopls, WorktreeRef},
};
use async_lsp::{
    LanguageServer, MainLoop, ServerSocket,
    lsp_types::{
        ClientCapabilities, DidOpenTextDocumentParams, HoverContents, HoverParams,
        InitializeParams, InitializedParams, Position, TextDocumentIdentifier, TextDocumentItem,
        TextDocumentPositionParams, Url, WorkDoneProgressParams, WorkspaceFolder,
    },
    router::Router,
};
use serde_json::Value;
use tokio::sync::{Barrier, Notify, oneshot};
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
        /// Distinguishes concurrently running real provider fixtures within one test process.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = env::temp_dir().join(format!(
            "agent-ide-gopls-contract-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&root)?;
        // `GoplsProfile` gives its listener these shared paths; unlike the production cache
        // lifecycle, this direct fixture must materialize them itself before the listener starts.
        fs::create_dir_all(root.join("cache/gopls"))?;
        fs::create_dir_all(root.join("cache/tmp"))?;
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

/// Creates one exact Workspace authority for a fixture worktree incarnation.
fn authority(root: &Path, id: &str, incarnation: &str) -> WorkspaceAuthority {
    WorkspaceAuthority::from_workspace(id.to_owned(), incarnation.to_owned(), root.to_path_buf(), 1)
        .expect("fixture root is absolute")
}

/// Creates the canonical Workspace identity and matching authority for one fixture incarnation.
fn worktree(root: &Path, incarnation: u64) -> (WorktreeRef, WorkspaceAuthority) {
    let worktree = WorktreeRef::from_discovery(
        root.to_path_buf(),
        root.to_path_buf(),
        PathBuf::from(".git"),
        incarnation,
    )
    .unwrap();
    let authority = authority(root, worktree.id(), &incarnation.to_string());
    (worktree, authority)
}

/// Admits one compatible registry view, reserving a heavy slot only for the first attachment.
fn provider_view(
    registry: &mut ProviderLeaseRegistry,
    admission: &mut AdmissionController,
    profile: &GoplsProfile,
    authority: &WorkspaceAuthority,
) -> ProviderViewLease {
    match registry.request(
        admission,
        OwnerId::new("listener").unwrap(),
        AdmissionClass::Interactive,
        profile.compatibility_key(),
        ProviderBackendKind::OwnedShared,
        authority,
    ) {
        ProviderLeaseAdmission::Granted(view) => view,
        outcome => panic!("expected compatible provider view: {outcome:?}"),
    }
}

/// Validates an admitted provider command for the fixture authority.
fn request(
    authority: WorkspaceAuthority,
    command: agent_ide::execution::ControlledCommand,
    program: &Path,
) -> ValidatedExecutionRequest {
    let policy = LocalExecutionPolicy::new(BTreeSet::from([program.to_path_buf()]), 4096, 8)
        .expect("test policy is valid");
    ValidatedExecutionRequest::validate(
        ValidatedHostInvocation::from_verified_binding("gopls-contract")
            .expect("test invocation is valid"),
        authority,
        command,
        &policy,
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

/// Records one forwarder's semantic result, post-detach result, and reaped Execution lease.
struct SessionResult {
    /// Semantic hover while both independently initialized forwarders remain live.
    initial_hover: String,
    /// Semantic hover after the left forwarder has completed provider-supported detachment.
    post_detach_hover: Option<String>,
    /// Execution admission lease released only after this forwarder was reaped.
    proof: Option<DirectChildReap>,
    /// Whether gopls reported its documented terminal remote-disconnect exit after LSP exit.
    terminal_remote_disconnect: bool,
}

/// Returns the bounded hover text for the fixture's duplicate `Shared` symbol.
async fn semantic_hover(
    server: &mut ServerSocket,
    file_uri: &Url,
) -> Result<String, Box<dyn std::error::Error>> {
    let response = tokio::time::timeout(
        DEADLINE,
        server.hover(HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: file_uri.clone(),
                },
                position: Position::new(2, 6),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        }),
    )
    .await??
    .ok_or_else(|| io::Error::other("gopls returned no hover"))?;
    Ok(match response.contents {
        HoverContents::Scalar(value) => format!("{value:?}"),
        HoverContents::Array(value) => format!("{value:?}"),
        HoverContents::Markup(value) => value.value,
    })
}

/// Builds a client router that observes server notifications without ending a valid session.
fn client_router() -> Router<()> {
    let mut router = Router::new(());
    router.unhandled_notification(|_, _| ControlFlow::Continue(()));
    router
}

/// Drives one forwarder through initialize/open, synchronized semantic work, and provider shutdown.
async fn run_session(
    mut child: OwnedProtocolChild,
    root: PathBuf,
    opened: Arc<Barrier>,
    begin_semantic: Arc<Barrier>,
    detached: Arc<Notify>,
    detach_after_initial_hover: bool,
    reaped_sender: Option<oneshot::Sender<DirectChildReap>>,
) -> Result<SessionResult, Box<dyn std::error::Error>> {
    let (mainloop, mut server) = MainLoop::new_client(|_| client_router());
    let root_uri =
        Url::from_file_path(&root).map_err(|_| io::Error::other("invalid fixture root URI"))?;
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
        tokio::time::timeout(DEADLINE, opened.wait())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "peer did not initialize"))?;
        tokio::time::timeout(DEADLINE, begin_semantic.wait())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "session check did not finish"))?;
        let initial_hover = semantic_hover(&mut server, &file_uri).await?;
        let post_detach_hover = if detach_after_initial_hover {
            None
        } else {
            tokio::time::timeout(DEADLINE, detached.notified())
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "left forwarder did not detach")
                })?;
            Some(semantic_hover(&mut server, &file_uri).await?)
        };
        tokio::time::timeout(DEADLINE, server.shutdown(())).await??;
        server.exit(())?;
        Ok::<_, Box<dyn std::error::Error>>((initial_hover, post_detach_hover))
    };
    let loop_run = mainloop.run_buffered(
        child.stdout.as_mut().unwrap().compat(),
        child.stdin.as_mut().unwrap().compat_write(),
    );
    let (exchange, loop_result) = tokio::join!(exchange, loop_run);
    let reaped = tokio::time::timeout(DEADLINE, child.reap(DEADLINE))
        .await?
        .map_err(execution_error)?;
    let status = reaped.status;
    let stderr = &reaped.stderr;
    let (initial_hover, post_detach_hover) = exchange?;
    let terminal_remote_disconnect = status.code() == Some(2)
        && String::from_utf8_lossy(&stderr.bytes)
            .contains("remote disconnected: failed reading header line: EOF");
    if !status.success() && !terminal_remote_disconnect {
        return Err(io::Error::other(format!(
            "gopls forwarder failed: {status}; stderr: {}",
            String::from_utf8_lossy(&stderr.bytes)
        ))
        .into());
    }
    match loop_result {
        Ok(()) | Err(async_lsp::Error::Eof) => {}
        Err(error) => return Err(error.into()),
    }
    let proof = if let Some(sender) = reaped_sender {
        sender
            .send(reaped.proof)
            .map_err(|_| io::Error::other("left-session coordinator stopped"))?;
        None
    } else {
        Some(reaped.proof)
    };
    Ok(SessionResult {
        initial_hover,
        post_detach_hover,
        proof,
        terminal_remote_disconnect,
    })
}

/// Runs the provider-supported daemon inspection command through an Execution-owned child.
async fn remote_sessions(
    profile: &GoplsProfile,
    socket: &Path,
    root: &Path,
    program: &Path,
    admission: &mut AdmissionController,
) -> Result<String, Box<dyn std::error::Error>> {
    let authority = authority(root, "sessions", "1");
    let request = request(
        authority.clone(),
        profile.sessions_command(&authority, socket)?,
        program,
    );
    let mut registry = ProviderLeaseRegistry::new(ProviderLeaseLimits {
        total_views: 1,
        per_backend_views: 1,
    })
    .unwrap();
    let ProviderLeaseAdmission::Granted(view) = registry.request(
        admission,
        OwnerId::new("sessions").unwrap(),
        AdmissionClass::Interactive,
        "gopls-session-inspection",
        ProviderBackendKind::OwnedExclusive,
        &authority,
    ) else {
        panic!("inspection admission");
    };
    let child = OwnedChild::spawn_from_provider_lease(
        &request,
        registry.take_spawn_lease(view).unwrap(),
        None,
        4096,
    )
    .map_err(execution_error)?;
    let reaped = tokio::time::timeout(DEADLINE, child.reap(DEADLINE, DEADLINE))
        .await?
        .map_err(execution_error)?;
    let cap = reap_capability(registry.release(view).unwrap());
    registry
        .complete_reap(admission, cap, reaped.settlement)
        .map_err(execution_error)?;
    if !reaped.evidence.status().success() {
        return Err(io::Error::other(format!(
            "gopls remote sessions failed: {}; stderr: {}",
            reaped.evidence.status(),
            String::from_utf8_lossy(&reaped.evidence.stderr().bytes)
        ))
        .into());
    }
    String::from_utf8(reaped.evidence.stdout().bytes.clone())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error).into())
}

/// Converts typed Execution failures into bounded test-context I/O errors.
/// Extracts the owned last-view capability without duplicating its settlement rights.
fn reap_capability(
    release: agent_ide::execution::BackendRelease,
) -> agent_ide::execution::BackendReapCapability {
    let agent_ide::execution::BackendRelease::ReapOwned(capability) = release else {
        panic!("owned reap capability")
    };
    capability
}

fn execution_error(error: impl std::fmt::Debug) -> io::Error {
    io::Error::other(format!(
        "Execution rejected gopls test operation: {error:?}"
    ))
}

/// Counts sessions that existed before the `remote sessions` inspection command connected.
fn forwarded_session_count(evidence: &str) -> io::Result<usize> {
    let value: Value = serde_json::from_str(evidence)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let clients = value
        .get("clients")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "gopls omitted clients"))?;
    let current = value
        .get("currentClientID")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "gopls omitted current client")
        })?;
    if clients
        .iter()
        .filter(|client| client.get("sessionID").and_then(Value::as_str) == Some(current))
        .count()
        != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "gopls current client is not exactly one reported session",
        ));
    }
    clients
        .len()
        .checked_sub(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "gopls reported no session"))
}

/// Rejects identity, incarnation, root and epoch substitution before either owned spawn path.
/// `/usr/bin/true` supplies harmless owned children only for the valid lifecycle endpoints.
#[tokio::test]
async fn gopls_spawns_require_exact_registry_authority() {
    let root = env::temp_dir();
    let (worktree, current) = worktree(&root, 1);
    let profile = GoplsProfile::new(
        PathBuf::from("/usr/bin/true"),
        "test".into(),
        "v0.1".into(),
        "test".into(),
        "/usr/bin/true".into(),
        "test".into(),
        root.join("gopls-authority-contract-cache")
            .display()
            .to_string(),
    )
    .unwrap();
    let socket = root.join("unused-authority-contract.sock");
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 3,
        per_owner_running: 1,
        per_owner_queued: 1,
        total_queued: 1,
        interactive_burst: 1,
    })
    .unwrap();
    let mut registry = ProviderLeaseRegistry::new(ProviderLeaseLimits {
        total_views: 2,
        per_backend_views: 2,
    })
    .unwrap();
    let mismatches = [
        WorkspaceAuthority::from_workspace("other-id", "1", root.clone(), 1).unwrap(),
        WorkspaceAuthority::from_workspace(current.worktree_id(), "2", root.clone(), 1).unwrap(),
        WorkspaceAuthority::from_workspace(current.worktree_id(), "1", root.join("other-root"), 1)
            .unwrap(),
        WorkspaceAuthority::from_workspace(current.worktree_id(), "1", root.clone(), 2).unwrap(),
    ];
    for mismatch in &mismatches {
        let view = provider_view(&mut registry, &mut admission, &profile, &current);
        let wrong = request(
            mismatch.clone(),
            profile.listener_command(mismatch, &socket).unwrap(),
            Path::new("/usr/bin/true"),
        );
        let error = SharedGopls::start(
            &profile,
            &wrong,
            registry.take_spawn_lease(view).unwrap(),
            None,
            64,
        )
        .err()
        .expect("listener authority mismatch must fail");
        let agent_ide::execution::ProcessError::NeverStarted { cause, settlement } = error else {
            panic!("missing no-child proof")
        };
        assert!(
            matches!(*cause,agent_ide::execution::ProcessError::Io(error) if error.kind()==io::ErrorKind::PermissionDenied)
        );
        assert!(matches!(
            registry.take_spawn_lease(view),
            Err(ProviderLeaseError::SpawnUnavailable)
        ));
        registry
            .settle_never_started(&mut admission, settlement)
            .unwrap();
        assert_eq!(admission.running_count(), 0);
    }
    let listener_view = provider_view(&mut registry, &mut admission, &profile, &current);
    let listener_request = request(
        current.clone(),
        profile.listener_command(&current, &socket).unwrap(),
        Path::new("/usr/bin/true"),
    );
    let mut shared = SharedGopls::start(
        &profile,
        &listener_request,
        registry.take_spawn_lease(listener_view).unwrap(),
        None,
        64,
    )
    .unwrap();
    let forwarder_request = request(
        current.clone(),
        profile.forwarder_command(&current, &socket).unwrap(),
        Path::new("/usr/bin/true"),
    );
    for mismatch in &mismatches {
        let view = provider_view(&mut registry, &mut admission, &profile, &current);
        let process = lease(&mut admission, "forwarder");
        let wrong = request(
            mismatch.clone(),
            profile.forwarder_command(mismatch, &socket).unwrap(),
            Path::new("/usr/bin/true"),
        );
        let (error, process) = registry
            .take_forwarder_spawn_lease(&mut admission, view, &wrong, process)
            .unwrap_err();
        assert_eq!(error, ProviderLeaseError::InvalidAuthority);
        assert_eq!(registry.forwarder_count(), 0);
        let capability = registry
            .take_forwarder_spawn_lease(&mut admission, view, &forwarder_request, process)
            .unwrap();
        let error = shared
            .open_view(worktree.clone(), 1, &wrong, capability, None, 64)
            .err()
            .expect("forwarder authority mismatch must fail");
        let agent_ide::execution::ProcessError::NeverStarted { cause, settlement } = error else {
            panic!("missing forwarder no-child proof")
        };
        assert!(
            matches!(*cause,agent_ide::execution::ProcessError::Io(error) if error.kind()==io::ErrorKind::PermissionDenied)
        );
        registry
            .settle_never_started(&mut admission, settlement)
            .unwrap();
        assert_eq!(shared.process_counts(), (1, 0, 0));
        registry.release(view).unwrap();
        assert_eq!(registry.forwarder_count(), 0);
    }
    let listener_process = shared
        .stop(Duration::from_millis(10), DEADLINE)
        .await
        .unwrap();
    let released = registry.release(listener_view).unwrap();
    registry
        .complete_reap(
            &mut admission,
            reap_capability(released),
            listener_process.settlement,
        )
        .unwrap();
    assert_eq!(registry.counts(), (0, 0));
    assert_eq!(registry.forwarder_count(), 0);
    assert_eq!(admission.running_count(), 0);
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
    let left_root = fixture.worktree("left");
    let right_root = fixture.worktree("right");
    let (left, left_authority) = worktree(&left_root, 1);
    let (right, right_authority) = worktree(&right_root, 2);
    let listener_command = profile
        .listener_command(&left_authority, &socket)
        .expect("listener command is declared");
    let mut listener_bound = provider::BoundRequest::new(
        "real-gopls-listener",
        left_authority.clone(),
        listener_command.clone(),
        &gopls,
    );
    let listener_debug = format!("{listener_command:?}");
    assert!(listener_debug.contains("-listen=unix;"));
    assert!(listener_debug.contains("-listen.timeout=10m"));
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 4,
        per_owner_running: 1,
        per_owner_queued: 1,
        total_queued: 1,
        interactive_burst: 1,
    })
    .unwrap();
    let mut registry = ProviderLeaseRegistry::new(ProviderLeaseLimits {
        total_views: 2,
        per_backend_views: 2,
    })
    .unwrap();
    let left_registry_view =
        provider_view(&mut registry, &mut admission, &profile, &left_authority);
    let listener_active = listener_bound.fresh();
    let mut shared = SharedGopls::start(
        &profile,
        &listener_bound.request,
        registry.take_spawn_lease(left_registry_view).unwrap(),
        Some(listener_active),
        4096,
    )
    .expect("Execution starts one listener");
    assert!(matches!(
        registry.take_spawn_lease(left_registry_view),
        Err(ProviderLeaseError::SpawnUnavailable)
    ));
    let ready = tokio::time::timeout(DEADLINE, async {
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    if ready.is_err() {
        let terminal = shared
            .stop(Duration::from_millis(100), Duration::from_secs(2))
            .await
            .unwrap();
        let cap = reap_capability(registry.release(left_registry_view).unwrap());
        registry
            .complete_reap(&mut admission, cap, terminal.settlement)
            .unwrap();
        panic!("listener did not bind: {:?}", terminal.evidence);
    }
    let mut left_bound = provider::BoundRequest::new(
        "real-gopls-left",
        left_authority.clone(),
        profile.forwarder_command(&left_authority, &socket).unwrap(),
        &gopls,
    );
    let mut right_bound = provider::BoundRequest::new(
        "real-gopls-right",
        right_authority.clone(),
        profile
            .forwarder_command(&right_authority, &socket)
            .unwrap(),
        &gopls,
    );
    let right_registry_view =
        provider_view(&mut registry, &mut admission, &profile, &right_authority);
    assert!(matches!(
        registry.take_spawn_lease(right_registry_view),
        Err(ProviderLeaseError::SpawnUnavailable)
    ));
    let left_process = lease(&mut admission, "left");
    let left_capability = registry
        .take_forwarder_spawn_lease(
            &mut admission,
            left_registry_view,
            &left_bound.request,
            left_process,
        )
        .unwrap();
    let right_process = lease(&mut admission, "right");
    let right_capability = registry
        .take_forwarder_spawn_lease(
            &mut admission,
            right_registry_view,
            &right_bound.request,
            right_process,
        )
        .unwrap();
    let left_active = left_bound.fresh();
    let left_view = shared
        .open_view(
            left.clone(),
            1,
            &left_bound.request,
            left_capability,
            Some(left_active),
            4096,
        )
        .unwrap();
    let right_active = right_bound.fresh();
    let right_view = shared
        .open_view(
            right.clone(),
            1,
            &right_bound.request,
            right_capability,
            Some(right_active),
            4096,
        )
        .unwrap();
    assert_eq!(shared.process_counts(), (1, 2, 2));
    assert_eq!(registry.counts(), (1, 2));
    assert_eq!(registry.forwarder_count(), 2);
    assert_eq!(admission.running_count(), 3);
    assert_eq!(shared.begin_request(&left, 1).unwrap(), 1);
    assert_eq!(shared.begin_request(&right, 1).unwrap(), 1);
    let left_lease = left_view.lease();
    let right_lease = right_view.lease();
    shared.observe_source(&left, left_lease, 2).unwrap();
    assert!(!shared.result_is_current(&left, left_lease, 1));
    assert!(shared.result_is_current(&left, left_lease, 2));
    assert!(shared.begin_request(&left, 1).is_err());
    assert_eq!(shared.begin_request(&left, 2).unwrap(), 2);
    assert!(shared.observe_source(&left, left_lease, 1).is_err());
    assert!(shared.result_is_current(&right, right_lease, 1));
    let opened = Arc::new(Barrier::new(3));
    let begin_semantic = Arc::new(Barrier::new(3));
    let detached = Arc::new(Notify::new());
    let (left_reaped_sender, left_reaped_receiver) = oneshot::channel();
    let sessions = async {
        let left_session = run_session(
            left_view.into_child(),
            left_root.clone(),
            Arc::clone(&opened),
            Arc::clone(&begin_semantic),
            Arc::clone(&detached),
            true,
            Some(left_reaped_sender),
        );
        let right_session = run_session(
            right_view.into_child(),
            right_root.clone(),
            Arc::clone(&opened),
            Arc::clone(&begin_semantic),
            Arc::clone(&detached),
            false,
            None,
        );
        let coordinator = async {
            tokio::time::timeout(DEADLINE, opened.wait())
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "forwarders did not initialize")
                })?;
            let evidence =
                remote_sessions(&profile, &socket, &left_root, &gopls, &mut admission).await?;
            assert_eq!(
                forwarded_session_count(&evidence)?,
                2,
                "two initialized forwarders must remain before inspection: {evidence}"
            );
            tokio::time::timeout(DEADLINE, begin_semantic.wait())
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "forwarders did not start semantics",
                    )
                })?;
            let left_admission = left_reaped_receiver
                .await
                .map_err(|_| io::Error::other("left forwarder did not reap"))?;
            shared.release_view(&left, left_lease)?;
            assert!(matches!(
                registry
                    .release(left_registry_view)
                    .map_err(execution_error)?,
                agent_ide::execution::BackendRelease::SharedPeerSurvives
            ));
            registry
                .complete_forwarder_reap(&mut admission, left_admission)
                .map_err(execution_error)?;
            assert_eq!(shared.process_counts(), (1, 1, 2));
            assert_eq!(registry.counts(), (1, 1));
            assert_eq!(registry.forwarder_count(), 1);
            assert_eq!(admission.running_count(), 2);
            assert!(!shared.result_is_current(&left, left_lease, 2));
            detached.notify_one();
            Ok::<_, Box<dyn std::error::Error>>(evidence)
        };
        tokio::try_join!(left_session, right_session, coordinator)
    };
    let (left_session, right_session, session_evidence) =
        tokio::time::timeout(Duration::from_secs(70), sessions)
            .await
            .expect("two sessions complete on deadline")
            .expect("two sessions satisfy the shared profile");
    assert!(
        session_evidence.contains("session"),
        "session evidence: {session_evidence}"
    );
    let left_hover = left_session.initial_hover;
    assert!(
        left_hover.contains("int"),
        "left semantic result: {left_hover}"
    );
    assert!(left_session.post_detach_hover.is_none());
    assert!(left_session.terminal_remote_disconnect);
    let right_hover = right_session.initial_hover;
    assert!(
        right_hover.contains("string"),
        "right semantic result: {right_hover}"
    );
    let right_after_detach = right_session
        .post_detach_hover
        .expect("right view remains initialized after left detaches");
    assert!(
        right_after_detach.contains("string"),
        "right post-detach semantic result: {right_after_detach}"
    );
    assert!(right_session.terminal_remote_disconnect);
    shared.release_view(&right, right_lease).unwrap();
    registry
        .complete_forwarder_reap(&mut admission, right_session.proof.unwrap())
        .unwrap();
    let listener_admission = shared
        .stop(Duration::from_millis(100), DEADLINE)
        .await
        .unwrap();
    let released = registry.release(right_registry_view).unwrap();
    registry
        .complete_reap(
            &mut admission,
            reap_capability(released),
            listener_admission.settlement,
        )
        .unwrap();
    assert_eq!(registry.counts(), (0, 0));
    assert_eq!(registry.forwarder_count(), 0);
    assert_eq!(admission.running_count(), 0);
    println!(
        "one listener, two initialized sessions, remote sessions=2, divergent int/string results, released left view, right post-detach string result, terminal remote disconnects=2; forwarders=2"
    );
}

/// A consumer failure before normal stop drops and kills its real listener, retaining uncertain admission.
#[tokio::test]
async fn dropping_live_gopls_owner_closes_its_owned_listener() {
    let fixture = Fixture::create().unwrap();
    let gopls = PathBuf::from(env::var("AGENT_IDE_GOPLS").unwrap());
    let root = fixture.worktree("left");
    let (_, authority) = worktree(&root, 1);
    let socket = fixture.root.join("drop.sock");
    let profile = GoplsProfile::new(
        gopls.clone(),
        "v0.23.0".into(),
        "v0.1".into(),
        "default".into(),
        env::var("AGENT_IDE_GO").unwrap(),
        "disabled-contract".into(),
        fixture.root.join("cache").display().to_string(),
    )
    .unwrap();
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 1,
        per_owner_running: 1,
        per_owner_queued: 1,
        total_queued: 1,
        interactive_burst: 1,
    })
    .unwrap();
    let mut registry = ProviderLeaseRegistry::new(ProviderLeaseLimits {
        total_views: 1,
        per_backend_views: 1,
    })
    .unwrap();
    let view = provider_view(&mut registry, &mut admission, &profile, &authority);
    let mut bound = provider::BoundRequest::new(
        "gopls-drop",
        authority.clone(),
        profile.listener_command(&authority, &socket).unwrap(),
        &gopls,
    );
    let active = bound.fresh();
    let shared = SharedGopls::start(
        &profile,
        &bound.request,
        registry.take_spawn_lease(view).unwrap(),
        Some(active),
        4096,
    )
    .unwrap();
    tokio::time::timeout(DEADLINE, async {
        while tokio::net::UnixStream::connect(&socket).await.is_err() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let failed_consumer: io::Result<()> = async move {
        let _owned_listener = shared;
        Err(io::Error::other(
            "consumer operation failed before normal stop",
        ))
    }
    .await;
    assert!(failed_consumer.is_err());
    tokio::time::timeout(Duration::from_secs(3), async {
        while tokio::net::UnixStream::connect(&socket).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("dropped owned gopls still listens");
    let _pending = reap_capability(registry.release(view).unwrap());

    assert_eq!(admission.running_count(), 1);
}

/// A relative cache namespace would resolve against the spawned child's working directory (the
/// worktree root) and write build/mod cache into the user's repository; the profile must refuse it.
#[test]
fn gopls_profile_rejects_a_relative_cache_namespace() {
    let relative = GoplsProfile::new(
        PathBuf::from("/usr/bin/true"),
        "v0.23.0".into(),
        "v0.1".into(),
        "contract-config".into(),
        "/usr/local/go".into(),
        "trusted".into(),
        "relative-cache-label".into(),
    );
    assert!(
        relative.is_err(),
        "relative cache_namespace must be rejected"
    );
    let absolute = GoplsProfile::new(
        PathBuf::from("/usr/bin/true"),
        "v0.23.0".into(),
        "v0.1".into(),
        "contract-config".into(),
        "/usr/local/go".into(),
        "trusted".into(),
        "/private/tmp/agent-ide-cache-namespace".into(),
    );
    assert!(
        absolute.is_ok(),
        "absolute cache_namespace must be accepted"
    );
}
