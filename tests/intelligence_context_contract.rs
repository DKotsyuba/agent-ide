//! Real gopls acceptance through the production Intelligence context/session API.

use agent_ide::{
    app::{
        config::StoreConfig,
        store::{OperationId, Store},
    },
    execution::{
        AdmissionClass, AdmissionController, AdmissionLimits, BackendRelease, CommandKind,
        ControlledCommand, ExecutionProfileCatalog, ExecutionProfileTemplate, HostSandboxState,
        LocalExecutionPolicy, OwnedProtocolChild, OwnerId, ProviderBackendKind,
        ProviderLeaseAdmission, ProviderLeaseLimits, ProviderLeaseRegistry,
        ValidatedExecutionRequest, ValidatedHostInvocation, WorkspaceAuthority,
    },
    intelligence::{
        context::{ContextMode, ContextQuery},
        freshness::{DiagnosticReadiness, Freshness, ViewGeneration},
        session::{GoEnv, ProviderSettings, SessionOptions, with_session},
    },
    workspace::{
        authority::WorktreeRef,
        observation::{
            ObservationRef, SourceBytes, SourceCoverage, SourceObservation, SourceRevision,
        },
        store::{ObservationAdmission, ObservationDraft, WorkspaceStore},
    },
};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::PathBuf,
    time::Duration,
};

/// Removes only this test's uniquely created provider fixture after owned process cleanup.
struct Fixture {
    /// Unique test-owned root holding the Go module, caches and Workspace database.
    root: PathBuf,
}
impl Fixture {
    /// Creates a Unicode/space-path Go module to verify raw URI conversion and provider source sync.
    fn new() -> Self {
        let root = env::temp_dir().join(format!("agent-ide-context-{} 🦀", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("go.mod"),
            "module contract.local/context\n\ngo 1.25.0\n",
        )
        .unwrap();
        Self { root }
    }
}
impl Drop for Fixture {
    /// Deletes test-owned files after the provider has been reaped or killed on drop.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Persists an exact source observation through Workspace, including bytes never read by the provider from disk.
async fn record(
    store: &WorkspaceStore<'_>,
    tree: &WorktreeRef,
    text: &str,
    number: u64,
) -> SourceObservation {
    match store
        .record(
            ObservationDraft::present(
                tree.clone(),
                1,
                OperationId::new(format!("observe-{number}")).unwrap(),
                ObservationRef::new(format!("observation-{number}")).unwrap(),
                "main.go".into(),
                SourceBytes::from_bytes(text.as_bytes()),
                SourceRevision::new(format!("source-{number}")).unwrap(),
                SourceCoverage::Complete,
            )
            .unwrap(),
        )
        .await
        .unwrap()
    {
        ObservationAdmission::Recorded(observation) => observation,
        other => panic!("expected a new exact Workspace observation: {other:?}"),
    }
}

/// Proves initialized capabilities, real definition/references, didChange overlays, source fences,
/// provisional real diagnostics, shutdown, and Execution-owned reaping without test-only LSP clients.
#[tokio::test]
async fn real_gopls_production_context_tracks_exact_observed_bytes() {
    let fixture = Fixture::new();
    let gopls = PathBuf::from(
        env::var("AGENT_IDE_GOPLS").expect("AGENT_IDE_GOPLS exact binary is required"),
    );
    let go =
        PathBuf::from(env::var("AGENT_IDE_GO").expect("AGENT_IDE_GO exact binary is required"));
    let tree =
        WorktreeRef::from_discovery(fixture.root.clone(), fixture.root.clone(), ".git".into(), 1)
            .unwrap();
    let initial =
        "package main\n// 🦀\nfunc Value() int { return 1 }\nfunc main() { _ = Value() }\n";
    let changed = "package main\n// 🦀\n\n\nfunc Value() int { return missing }\nfunc main() { _ = Value() }\n";
    fs::write(fixture.root.join("main.go"), initial).unwrap();
    let store = Store::open(
        &fixture.root.join("observations.sqlite"),
        StoreConfig {
            queue_capacity: 8,
            busy_timeout: Duration::from_secs(1),
            request_deadline: Duration::from_secs(2),
            receipt_capacity: 8,
        },
    )
    .unwrap();
    let workspace = WorkspaceStore::new(&store);
    workspace.install_schema().await.unwrap();
    let first = record(&workspace, &tree, initial, 1).await;
    let second = record(&workspace, &tree, changed, 2).await;
    let missing = match workspace
        .record(
            ObservationDraft::missing(
                tree.clone(),
                1,
                OperationId::new("missing").unwrap(),
                ObservationRef::new("missing").unwrap(),
                "main.go".into(),
                SourceRevision::new("missing").unwrap(),
                SourceCoverage::Complete,
            )
            .unwrap(),
        )
        .await
        .unwrap()
    {
        ObservationAdmission::Recorded(observation) => observation,
        other => panic!("expected missing-path observation: {other:?}"),
    };
    let reopened = record(&workspace, &tree, initial, 4).await;
    let authority = WorkspaceAuthority::from_workspace(
        tree.id(),
        tree.incarnation().to_string(),
        fixture.root.clone(),
        1,
    )
    .unwrap();
    let command = ControlledCommand::from_validated_peer(
        CommandKind::Provider,
        gopls.clone(),
        vec![],
        fixture.root.clone(),
        BTreeMap::from([
            ("PATH".into(), go.parent().unwrap().as_os_str().to_owned()),
            (
                "GOCACHE".into(),
                fixture.root.join("go-cache").into_os_string(),
            ),
            (
                "GOMODCACHE".into(),
                fixture.root.join("module-cache").into_os_string(),
            ),
            (
                "GOPATH".into(),
                fixture.root.join("go-path").into_os_string(),
            ),
        ]),
    )
    .unwrap();
    let sandbox = HostSandboxState::parse(Some(json!({"permissionProfile":{"type":"disabled"},"codexLinuxSandboxExe":null,"sandboxCwd":fixture.root}))).unwrap();
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence("context-test", 1, &sandbox).unwrap(),
    ])
    .unwrap();
    let request = ValidatedExecutionRequest::validate(
        ValidatedHostInvocation::from_verified_binding("context-test", sandbox).unwrap(),
        authority,
        command,
        &LocalExecutionPolicy::new(BTreeSet::from([gopls]), 4096, 4, true).unwrap(),
        &catalog,
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
    let ProviderLeaseAdmission::Granted(view) = registry.request(
        &mut admission,
        OwnerId::new("context-test").unwrap(),
        AdmissionClass::Interactive,
        "context-gopls",
        ProviderBackendKind::OwnedExclusive,
        request.authority(),
    ) else {
        panic!("provider admission");
    };
    let mut child = OwnedProtocolChild::spawn_from_provider_lease(
        &request,
        registry.take_spawn_lease(view).unwrap(),
        None,
        &PathBuf::from("/unused"),
        4096,
    )
    .unwrap();
    let result = with_session(
        &mut child.stdout,
        &mut child.stdin,
        tree,
        1,
        ViewGeneration {
            backend: 1,
            configuration: 1,
            toolchain: 1,
            view: 1,
        },
        ProviderSettings::GoplsDefaults(
            GoEnv::prepare(
                fixture.root.join("go-cache"),
                fixture.root.join("module-cache"),
                fixture.root.join("go-tmp"),
            )
            .expect("absolute private go namespace"),
        ),
        SessionOptions {
            request_timeout: Duration::from_secs(30),
            lifetime: Duration::from_secs(90),
        },
        |mut session| async move {
            assert!(
                session
                    .capabilities()
                    .advertised
                    .definition_provider
                    .is_some()
            );
            assert!(
                session
                    .capabilities()
                    .advertised
                    .references_provider
                    .is_some()
            );
            assert_eq!(
                session.diagnostics().readiness,
                DiagnosticReadiness::Unknown
            );
            let query = ContextQuery::Symbol {
                byte_offset: initial.rfind("Value").unwrap(),
            };
            let before = session.context(&first, initial.as_bytes(), query).await?;
            assert_eq!(before.mode, ContextMode::Semantic, "{before:?}");
            assert_eq!(before.document_version, Some(1));
            assert_eq!(before.definitions.as_ref().unwrap()[0].range.start.line, 2);
            assert_eq!(before.references.as_ref().unwrap().len(), 2);
            assert_eq!(before.definitions.as_ref().unwrap()[0].uri, before.uri);
            assert_eq!(before.freshness, Freshness::Current);
            let after = session
                .context(
                    &second,
                    changed.as_bytes(),
                    ContextQuery::Symbol {
                        byte_offset: changed.rfind("Value").unwrap(),
                    },
                )
                .await?;
            assert_eq!(after.mode, ContextMode::Semantic, "{after:?}");
            assert_eq!(after.document_version, Some(2));
            assert_eq!(after.definitions.as_ref().unwrap()[0].range.start.line, 4);
            assert_eq!(after.references.as_ref().unwrap().len(), 2);
            assert!(
                session
                    .context(&first, initial.as_bytes(), query)
                    .await
                    .is_err()
            );
            assert!(
                session
                    .context(&second, initial.as_bytes(), query)
                    .await
                    .is_err()
            );
            tokio::time::timeout(Duration::from_secs(20), async {
                while session.diagnostics().diagnostics.is_empty() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("real gopls must publish the missing-name diagnostic");
            let diagnostics = session.diagnostics();
            assert_eq!(diagnostics.freshness, Freshness::Provisional);
            assert_eq!(diagnostics.readiness, DiagnosticReadiness::Unknown);
            assert!(
                diagnostics
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.message.contains("missing"))
            );
            let closed = session.context(&missing, &[], ContextQuery::File).await?;
            assert!(closed.document_version.is_none());
            assert!(matches!(closed.mode, ContextMode::Lexical { .. }));
            assert!(session.diagnostics().diagnostics.is_empty());
            assert_eq!(
                session.diagnostics().readiness,
                DiagnosticReadiness::Unknown
            );
            let reopened = session
                .context(&reopened, initial.as_bytes(), query)
                .await?;
            assert_eq!(reopened.mode, ContextMode::Semantic);
            assert_eq!(reopened.document_version, Some(3));
            assert_eq!(
                reopened.definitions.as_ref().unwrap()[0].range.start.line,
                2
            );
            session.shutdown().await?;
            Ok(())
        },
    )
    .await;
    let reaped = tokio::time::timeout(Duration::from_secs(30), child.reap(Duration::from_secs(2)))
        .await
        .unwrap()
        .unwrap();
    assert!(
        reaped.status.success(),
        "gopls failed: {}; {}",
        reaped.status,
        String::from_utf8_lossy(&reaped.stderr.bytes)
    );
    let BackendRelease::ReapOwned(cap) = registry.release(view).unwrap() else {
        panic!("owned backend");
    };
    registry
        .complete_reap(&mut admission, cap, reaped.proof)
        .unwrap();
    assert_eq!(admission.running_count(), 0);
    result.unwrap();
    assert_eq!(
        fs::read_to_string(fixture.root.join("main.go")).unwrap(),
        initial,
        "Intelligence must not write observed overlay bytes"
    );
}
