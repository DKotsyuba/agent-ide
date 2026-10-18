//! Focused post-review regressions for current receipt proof, deferred provider fallback and
//! useful-result accounting. Every runtime and synthetic provider belongs to its fixture.

use super::*;

/// Serves qualified SDK admission and records actual publication attempts. Refusing custody is
/// deliberate: assertions cover the authorized Bridge payload, never claim native delivery.
async fn qualified_bridge(
    fixture: &ProductFixture,
) -> (
    tokio::task::JoinSet<()>,
    tokio::sync::mpsc::UnboundedReceiver<Value>,
    std::sync::Arc<AtomicUsize>,
) {
    let token = fixture.base.join("token");
    std::fs::set_permissions(&fixture.base, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(&token, "fixture-token").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["bridge"] = json!({"registration_socket":fixture.base.join("a.sock"),"core_socket":fixture.base.join("b.sock"),
        "credential_file":token,"native_stores":[{"harness":"codex","host":"local","store":"fixture"}]});
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    let capabilities = std::sync::Arc::new(AtomicUsize::new(0));
    let (send, receive) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = tokio::task::JoinSet::new();
    for socket in ["a.sock", "b.sock"] {
        let listener = tokio::net::UnixListener::bind(fixture.base.join(socket)).unwrap();
        let send = send.clone();
        let capabilities = capabilities.clone();
        tasks.spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let response = if request["op"] == "capabilities" {
                    capabilities.fetch_add(1, Ordering::Relaxed);
                    json!({"api_version":1,"request_id":request["request_id"],"success":true,
                    "result":{"op":"capabilities","value":{"api_version":1,"supported_versions":[1],"operations":["publish","retire_delivery_scope"],
                        "evidence_variants":[],"native_contracts":[],"modes":["auto"],"policies":["keep_all"],"reply_policies":[],
                        "limits":{"registration_request_bytes":65536,"core_request_bytes":65536,"response_bytes":65536,"payload_bytes":32768,"metadata_entries":0,"rendered_manifest_bytes":65536,"open_routes":0,"assertions":0},
                        "grant":{"allowed_modes":["auto"],"allowed_policies":["keep_all"],"allowed_kinds":["*"],"reply_policies":[],"direct_address":true,"native_scopes":["*"]},
                        "features":{"consume_assertion":false,"delivery_scopes":true,"host_observations":false},
                        "retention":{"durable_keys":true,"closed_tombstones":true,"credential_independent_identity":true}}}})
                } else {
                    if request["op"] == "publish" { let _ = send.send(request.clone()); }
                    json!({"api_version":1,"request_id":request["request_id"],"success":false,
                        "error":{"class":"capacity","reason":"fixture","retry_hint":"never","effect":"not_committed","detail":"fixture"}})
                };
                stream.get_mut().write_all(format!("{response}\n").as_bytes()).await.unwrap();
            }
        });
    }
    (tasks, receive, capabilities)
}

/// Installs a bounded synthetic LSP process using only macOS Perl/JSON::PP. It speaks actual
/// initialize/status/document/rename frames. A changed document first sends a stale clean push,
/// then holds the current error push behind `gate`; harmless observation rebinding never holds.
/// This is a fixed ASCII test peer, not a production parser or another installed dependency.
fn held_provider(fixture: &ProductFixture, gate: &Path) {
    let program = fixture.base.join("provider");
    let script = r#"#!/usr/bin/perl
use strict; use warnings; use JSON::PP;
binmode STDIN; binmode STDOUT; $|=1;
my $gate='@GATE@'; my ($text,$uri)=('','');
sub send_json { my $s=encode_json($_[0]); print 'Content-Length: '.length($s)."\r\n\r\n".$s; }
sub diagnostics {
 my ($version,$changed)=@_;
 if ($changed && -e $gate) {
  send_json({jsonrpc=>'2.0',method=>'textDocument/publishDiagnostics',params=>{uri=>$uri,version=>$version-1,diagnostics=>[]}});
  open(my $seen,'>',$gate.'.seen') or die $!; print $seen $version; close($seen);
  while (-e $gate) { select(undef,undef,undef,0.01); }
 }
 my $d=$changed ? [{range=>{start=>{line=>0,character=>0},end=>{line=>0,character=>1}},severity=>1,code=>'E0308',message=>'fixture post-write type error'}] : [];
 send_json({jsonrpc=>'2.0',method=>'textDocument/publishDiagnostics',params=>{uri=>$uri,version=>$version,diagnostics=>$d}});
}
while (my $header=<STDIN>) {
 next unless $header =~ /^Content-Length: (\d+)/i; my $n=$1;
 while (my $h=<STDIN>) { last if $h eq "\r\n" || $h eq "\n"; }
 my $body=''; while(length($body)<$n) { my $part=''; my $r=read(STDIN,$part,$n-length($body)); exit unless $r; $body.=$part; }
 my $r=decode_json($body); my $m=$r->{method}//''; my $p=$r->{params}//{}; my $out;
 if ($m eq 'initialize') { $out={capabilities=>{positionEncoding=>'utf-8',textDocumentSync=>2,definitionProvider=>JSON::PP::true,referencesProvider=>JSON::PP::true,documentSymbolProvider=>JSON::PP::true,hoverProvider=>JSON::PP::true,renameProvider=>JSON::PP::true},serverInfo=>{name=>'rust-analyzer',version=>'1.98.1 (48a229ce 2026-09-01)'}}; }
 elsif ($m eq 'initialized') { send_json({jsonrpc=>'2.0',method=>'experimental/serverStatus',params=>{health=>'ok',quiescent=>JSON::PP::true,message=>undef}}); next; }
 elsif ($m eq 'textDocument/didOpen') { $text=$p->{textDocument}{text}; $uri=$p->{textDocument}{uri}; diagnostics($p->{textDocument}{version},0); next; }
 elsif ($m eq 'textDocument/didChange') { my $next=$p->{contentChanges}[0]{text}; my $changed=$next ne $text; $text=$next; $uri=$p->{textDocument}{uri}; diagnostics($p->{textDocument}{version},$changed); next; }
 elsif ($m eq 'textDocument/documentSymbol') {
  my @rows; my @lines=split(/\n/,$text,-1);
  for(my $i=0;$i<@lines;$i++) { if($lines[$i]=~/fn\s+(\w+)/) { my $name=$1; my $at=$-[1]; push @rows,{name=>$name,kind=>12,range=>{start=>{line=>$i,character=>0},end=>{line=>$i,character=>length($lines[$i])}},selectionRange=>{start=>{line=>$i,character=>$at},end=>{line=>$i,character=>$at+length($name)}}}; } }
  $out=\@rows;
 }
 elsif ($m eq 'textDocument/rename') {
  my @edits; my @lines=split(/\n/,$text,-1);
  for(my $i=0;$i<@lines;$i++) { while($lines[$i]=~/\bvalue\b/g) { push @edits,{range=>{start=>{line=>$i,character=>$-[0]},end=>{line=>$i,character=>$+[0]}},newText=>$p->{newName}}; } }
  $out={changes=>{$uri=>\@edits}};
 }
 elsif ($m eq 'textDocument/definition' || $m eq 'textDocument/references' || $m =~ /Calls$/) { $out=[]; }
 elsif ($m eq 'exit') { exit; }
 else { $out=undef; }
 send_json({jsonrpc=>'2.0',id=>$r->{id},result=>$out}) if exists($r->{id});
}
"#.replace("@GATE@", gate.to_str().unwrap());
    std::fs::write(&program, script).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture.write_config(json!([{"executable":accepted_program(program.to_str().unwrap(),"rust-analyzer 1.98.1 (48a229ce 2026-09-01)"),
        "settings":"rust_cache_priming_disabled_v1","toolchain":"stable","cargo":accepted_program("/usr/bin/true","cargo 1.98.1"),"cargo_version":"cargo 1.98.1",
        "rustc":accepted_program("/usr/bin/true","rustc 1.98.1"),"rustc_version":"rustc 1.98.1","trust":"fixture-disabled","cache_namespace":"held-diagnostics"}]));
}

/// Waits on the provider's actual post-write synchronization checkpoint; elapsed time is only
/// an outer hang bound and never the proof that a current push has been held.
async fn held_push_seen(gate: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !gate.with_extension("seen").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("provider did not reach held post-write diagnostic push");
}

/// A completed, clean batch cannot replay current diagnostics/new source after a native change
/// or deletion of a secondary file. Historical write rows stay; retries never restore bytes.
#[tokio::test]
async fn terminal_batch_retry_requires_complete_current_sources() {
    for delete in [false, true] {
        let fixture = ProductFixture::new(json!([]));
        std::fs::write(
            fixture.root.join("src/extra.rs"),
            "pub fn extra() -> i32 { 1 }\n",
        )
        .unwrap();
        std::fs::write(
            fixture.root.join("src/lib.rs"),
            "pub mod extra;\npub fn value() -> i32 { 7 }\n",
        )
        .unwrap();
        fixture.git(&["add", "--", "src/lib.rs", "src/extra.rs"]);
        fixture.git(&["commit", "--quiet", "-m", "complete-set fixture"]);
        let home = enable_fake_rust_checks(&fixture, &fixture.base);
        let mut daemon = fixture.daemon_with_home(Some(&home)).await;
        let mut actor = ProductActor::new(&fixture, "terminal-current-proof").await;
        let start = actor.call(&fixture, "ide.start", json!({})).await;
        assert_eq!(actor.settle(&fixture, start).await["kind"], "activation");
        let parameters = json!({"operation_id":"complete-batch","files":[
            {"path":"src/lib.rs","changes":[{"old":"{ 7 }","new":"{ 8 }"}]},
            {"path":"src/extra.rs","changes":[{"old":"{ 1 }","new":"{ 2 }"}]}]});
        let first = actor.call(&fixture, "ide.edit", parameters.clone()).await;
        let first = actor.settle(&fixture, first).await;
        let reference = first["source_ref"]
            .as_str()
            .expect("batch receipt")
            .to_owned();
        let terminal = actor
            .call(
                &fixture,
                "ide.inspect",
                json!({"detail_ref":reference,"wait_s":30}),
            )
            .await;
        assert!(
            terminal["text"]
                .as_str()
                .unwrap()
                .contains("diagnostics: clean"),
            "{terminal}"
        );
        let before = std::fs::metadata(fixture.root.join("src/lib.rs"))
            .unwrap()
            .modified()
            .unwrap();
        let same = actor.call(&fixture, "ide.edit", parameters.clone()).await;
        assert_eq!(same["outcome"], "applied", "{same}");
        assert!(
            same["text"].as_str().unwrap().contains("written earlier"),
            "{same}"
        );
        if delete {
            std::fs::remove_file(fixture.root.join("src/extra.rs")).unwrap();
        } else {
            std::fs::write(fixture.root.join("src/extra.rs"), "pub fn native() {}\n").unwrap();
        }
        let stale = actor.call(&fixture, "ide.edit", parameters).await;
        assert_eq!(stale["code"], "source_unavailable", "{stale}");
        assert!(stale.get("source_ref").is_none(), "{stale}");
        let old = actor
            .call(&fixture, "ide.inspect", json!({"detail_ref":reference}))
            .await;
        assert_eq!(
            old["code"], "source_unavailable",
            "secondary proof must fence retained result too: {old}"
        );
        assert_eq!(
            std::fs::metadata(fixture.root.join("src/lib.rs"))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
        if delete {
            assert!(!fixture.root.join("src/extra.rs").exists());
        } else {
            assert_eq!(
                std::fs::read_to_string(fixture.root.join("src/extra.rs")).unwrap(),
                "pub fn native() {}\n"
            );
        }
        actor.call(&fixture, "ide.stop", json!({})).await;
        actor.mcp.close().await;
        daemon.kill().await.unwrap();
        daemon.wait().await.unwrap();
    }
}

/// A qualified ready provider's current error survives unavailable project checks after a
/// prompt receipt. Held/stale pushes cannot become clean; a plain source read remains independent.
/// Both single changes and successful rename use the same fallback stage and retained identity.
#[tokio::test]
async fn ready_provider_fallback_is_post_receipt_and_does_not_block_plain_read() {
    for rename in [false, true] {
        let fixture = ProductFixture::new(json!([]));
        let gate = fixture.base.join("push-gate");
        held_provider(&fixture, &gate);
        let (mut bridge, mut publications, capabilities) = qualified_bridge(&fixture).await;
        let mut daemon = fixture
            .daemon_with_home(Some(&provider_home(&fixture)))
            .await;
        let mut actor = ProductActor::new(&fixture, "ready-fallback").await;
        let start = actor.call(&fixture, "ide.start", json!({})).await;
        assert_eq!(actor.settle(&fixture, start).await["kind"], "activation");
        batch_wait_semantic_provider(&mut actor, &fixture, "src/lib.rs").await;
        std::fs::write(&gate, "hold").unwrap();
        let parameters = if rename {
            json!({"operation_id":"provider-fallback","op":"rename","symbol":"src/lib.rs#value","new_name":"renamed"})
        } else {
            json!({"operation_id":"provider-fallback","path":"src/lib.rs","changes":[{"old":"{ 7 }","new":"{ true }"}]})
        };
        let receipt = tokio::time::timeout(
            Duration::from_secs(5),
            actor.call(&fixture, "ide.edit", parameters.clone()),
        )
        .await
        .expect("provider pushes cannot block durable receipt");
        assert_eq!(receipt["result"]["outcome"], "replaced", "{receipt}");
        assert_eq!(receipt["diagnostics"]["state"], "pending", "{receipt}");
        assert!(
            capabilities.load(Ordering::Relaxed) > 0,
            "fallback must preserve prewrite Bridge capability admission"
        );
        let reference = receipt["result"]["source_ref"].as_str().unwrap().to_owned();
        held_push_seen(&gate).await;
        let began = std::time::Instant::now();
        let read = tokio::time::timeout(
            Duration::from_secs(5),
            actor.call(
                &fixture,
                "ide.read",
                json!({"path":"src/lib.rs","force":true}),
            ),
        )
        .await
        .expect("held diagnostics cannot own the serial source worker");
        assert_eq!(read["kind"], "read", "{read}");
        assert!(
            began.elapsed() < Duration::from_millis(1500),
            "snapshot polling must not wait on the held push: {:?}",
            began.elapsed()
        );
        assert!(
            publications.try_recv().is_err(),
            "stale clean snapshot is not a terminal fact"
        );
        let retry = actor.call(&fixture, "ide.edit", parameters).await;
        assert_eq!(retry["result"]["source_ref"], reference, "{retry}");
        std::fs::remove_file(&gate).unwrap();
        let terminal = actor
            .call(
                &fixture,
                "ide.inspect",
                json!({"detail_ref":reference,"wait_s":30}),
            )
            .await;
        assert_eq!(
            terminal["diagnostics"]["state"], "current_reported",
            "{terminal}"
        );
        assert!(
            terminal["diagnostics"]["messages"]
                .to_string()
                .contains("fixture post-write type error"),
            "{terminal}"
        );
        let publication = tokio::time::timeout(Duration::from_secs(5), publications.recv())
            .await
            .unwrap()
            .expect("one useful accepted fallback terminal attempt");
        assert!(
            publication.to_string().contains("edit-checked")
                && publication
                    .to_string()
                    .contains("fixture post-write type error"),
            "{publication}"
        );
        for settled in [&terminal, &publication] {
            assert!(
                !settled.to_string().contains("diagnostics pending"),
                "terminal retained/publication note must be settlement-neutral: {settled}"
            );
        }
        actor.call(&fixture, "ide.stop", json!({})).await;
        actor.mcp.close().await;
        daemon.kill().await.unwrap();
        daemon.wait().await.unwrap();
        bridge.abort_all();
        while let Some(result) = bridge.join_next().await {
            assert!(result.is_err_and(|error| error.is_cancelled()));
        }
    }
}

/// Reads the fixture daemon's own journal, excluding other test homes and non-usable stage rows.
fn usable_events(home: &Path) -> Vec<agent_ide::errorlog::LoggedEvent> {
    std::fs::read_dir(home.join(".agent-ide/logs"))
        .unwrap()
        .flatten()
        .flat_map(|entry| agent_ide::errorlog::read_events(&entry.path()))
        .filter(|event| {
            event
                .detail
                .as_deref()
                .is_some_and(|detail| detail.starts_with("usable_result;"))
        })
        .collect()
}

/// Actual missing reads and precommit syntax refusals retain failure-stage records without
/// usable-result counters; a real source and confirmed write still each record one first-use time.
#[tokio::test]
async fn usable_journal_requires_actual_source_or_confirmed_receipt() {
    let fixture = ProductFixture::new(json!([]));
    let home = provider_home(&fixture);
    let mut daemon = fixture.daemon_with_home(Some(&home)).await;
    let mut actor = ProductActor::new(&fixture, "usable-journal").await;
    let start = actor.call(&fixture, "ide.start", json!({})).await;
    assert_eq!(actor.settle(&fixture, start).await["kind"], "activation");
    let missing = actor
        .call(&fixture, "ide.read", json!({"path":"src/missing.rs"}))
        .await;
    assert_eq!(missing["state"], "error", "{missing}");
    assert_eq!(
        missing["code"]["no_such_file"], "src/missing.rs",
        "{missing}"
    );
    assert!(usable_events(&home).is_empty());
    let before = std::fs::read(fixture.root.join("src/lib.rs")).unwrap();
    let refusal = actor.call(&fixture, "ide.edit", json!({"operation_id":"syntax-refusal","path":"src/lib.rs","changes":[{"old":"{ 7 }","new":"{"}]})).await;
    let refusal = actor.settle(&fixture, refusal).await;
    assert_eq!(refusal["code"], "edit_refused", "{refusal}");
    assert_eq!(
        std::fs::read(fixture.root.join("src/lib.rs")).unwrap(),
        before
    );
    assert!(usable_events(&home).is_empty(), "refusal is not usable");
    let all = std::fs::read_dir(home.join(".agent-ide/logs"))
        .unwrap()
        .flatten()
        .flat_map(|entry| agent_ide::errorlog::read_events(&entry.path()))
        .collect::<Vec<_>>();
    assert!(
        all.iter().any(|event| event
            .detail
            .as_deref()
            .is_some_and(|detail| detail.starts_with("stage: gate;"))),
        "failure stage timing remains: {all:?}"
    );
    assert!(
        all.iter()
            .any(|event| event.reason.as_deref() == Some("no_such_file")
                && event.outcome == "failed"),
        "missing read keeps terminal failure status: {all:?}"
    );
    let read = actor
        .call(
            &fixture,
            "ide.read",
            json!({"path":"src/lib.rs","force":true}),
        )
        .await;
    assert_eq!(read["kind"], "read", "{read}");
    assert_eq!(usable_events(&home).len(), 1, "actual source is useful");
    let written = actor.call(&fixture, "ide.edit", json!({"operation_id":"usable-write","path":"src/lib.rs","changes":[{"old":"{ 7 }","new":"{ 8 }"}]})).await;
    let written = actor.settle(&fixture, written).await;
    assert_eq!(written["result"]["outcome"], "replaced", "{written}");
    let useful = usable_events(&home);
    assert_eq!(useful.len(), 2, "source and receipt only: {useful:?}");
    assert!(
        useful
            .iter()
            .all(|event| event.duration_ms.is_some_and(|duration| duration < 5000))
    );
    actor.call(&fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}
