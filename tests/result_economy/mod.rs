//! Product contracts for compact results and explicit, once-delivered test output pages.

use super::*;

/// Serves the real SDK's capability admission and refuses mutation custody deterministically.
/// The caller aborts and joins this fixture task after inspecting the daemon's publication journal.
async fn bridge_fixture(core: &Path) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::UnixListener::bind(core).unwrap();
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let response = if request["op"] == "capabilities" {
                json!({"api_version":1,"request_id":request["request_id"],"success":true,
                "result":{"op":"capabilities","value":{
                    "api_version":1,"supported_versions":[1],"operations":["publish","retire_delivery_scope"],
                    "evidence_variants":[],"native_contracts":[],"modes":["auto"],"policies":["keep_all"],"reply_policies":[],
                    "limits":{"registration_request_bytes":65536,"core_request_bytes":65536,"response_bytes":65536,"payload_bytes":32768,"metadata_entries":0,"rendered_manifest_bytes":65536,"open_routes":0,"assertions":0},
                    "grant":{"allowed_modes":["auto"],"allowed_policies":["keep_all"],"allowed_kinds":["*"],"reply_policies":[],"direct_address":true,"native_scopes":["*"]},
                    "features":{"consume_assertion":false,"delivery_scopes":true,"host_observations":false},
                    "retention":{"durable_keys":true,"closed_tombstones":true,"credential_independent_identity":true}
                }}})
            } else {
                json!({"api_version":1,"request_id":request["request_id"],"success":false,
                    "error":{"class":"capacity","reason":"fixture","retry_hint":"never","effect":"not_committed","detail":"fixture"}})
            };
            let bytes = format!("{response}\n");
            socket.get_mut().write_all(bytes.as_bytes()).await.unwrap();
        }
    })
}

/// Counts durable Bridge publication attempts in this fixture, irrespective of socket custody.
fn bridge_publications(fixture: &ProductFixture) -> usize {
    let database = rusqlite::Connection::open(fixture.runtime.join("state.sqlite")).unwrap();
    let exists: bool = database.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='ide_bridge_delivery')", [], |row| row.get(0)).unwrap();
    if !exists {
        return 0;
    }
    database
        .query_row("SELECT count(*) FROM ide_bridge_delivery", [], |row| {
            row.get(0)
        })
        .unwrap()
}

/// A missing configured cargo binary produces inline established unavailability and no wake;
/// a working check still settles with its diagnostics and records a deferred check publication.
#[tokio::test]
async fn unavailable_checks_stay_inline_and_working_checks_still_publish() {
    for broken in [true, false] {
        let fixture = ProductFixture::new(json!([]));
        std::fs::set_permissions(&fixture.base, std::fs::Permissions::from_mode(0o700)).unwrap();
        let gate = hold_checks_at_gate(&fixture);
        let home = enable_fake_rust_checks_holding(&fixture, &fixture.base, &gate);
        release_checks_gate(&fixture);
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
        if broken {
            config["project_checks"]["rust"]["toolchain_dir"] =
                json!(home.join(".rustup/toolchains/missing"));
        }
        let token = fixture.base.join("bridge-token");
        std::fs::write(&token, "fixture-token").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        config["bridge"] = json!({
            "registration_socket":fixture.base.join("bridge-adapter.sock"),
            "core_socket":fixture.base.join("bridge-core.sock"),
            "credential_file":token,
            "native_stores":[{"harness":"codex","host":"local","store":"fixture"}]
        });
        std::fs::write(&fixture.config, config.to_string()).unwrap();
        let bridge_servers = [
            bridge_fixture(&fixture.base.join("bridge-core.sock")).await,
            bridge_fixture(&fixture.base.join("bridge-adapter.sock")).await,
        ];
        let mut daemon = fixture.daemon_with_home(Some(&home)).await;
        let mut actor = ProductActor::new(&fixture, "check-availability").await;
        let started = actor.call(&fixture, "ide.start", json!({})).await;
        assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let problems = actor
                .call(&fixture, "ide.inspect", json!({"kind":"problems"}))
                .await;
            let text = problems["text"].as_str().unwrap_or_default();
            if (broken && text.contains("tool not found"))
                || (!broken && text.contains("rust: ready"))
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "check never settled: {problems}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(bridge_publications(&fixture), 0);
        let read = actor
            .call(
                &fixture,
                "ide.read",
                json!({"items":[{"path":"src/lib.rs"}]}),
            )
            .await;
        let read = actor.settle(&fixture, read).await;
        if !broken {
            std::fs::remove_file(fixture.base.join(CHECKS_GATE)).unwrap();
            write_problems_count(&fixture.root, "1");
        }
        let edit = batch_call_with_text(
            &mut actor,
            &fixture,
            "ide.edit",
            json!({
                "path":"src/lib.rs", "source_ref":read["detail_ref"],
                "operation_id":"availability-edit", "changes":[{"old":"{ 7 }","new":"{ 8 }"}]
            }),
        )
        .await;
        let (edit, rendered) = if broken {
            batch_settle_with_text(&mut actor, &fixture, edit).await
        } else {
            edit
        };
        if broken {
            assert_eq!(edit["diagnostics"]["state"], "not_analysed", "{edit}");
            let reason = edit["diagnostics"]["reason"].as_str().unwrap();
            assert!(
                reason.contains("checks unavailable:") && reason.contains("tool not found"),
                "{edit}"
            );
            assert_eq!(
                rendered.matches("checks unavailable:").count(),
                1,
                "{rendered}"
            );
            assert_eq!(
                bridge_publications(&fixture),
                0,
                "unavailable check must not publish"
            );
        } else {
            assert_eq!(edit["diagnostics"]["state"], "pending", "{edit}");
            release_checks_gate(&fixture);
            let reference = edit["diagnostics"]["detail_ref"].as_str().unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                let result = actor
                    .call(&fixture, "ide.inspect", json!({"detail_ref":reference}))
                    .await;
                if result["diagnostics"]["state"] == "current_reported"
                    && bridge_publications(&fixture) > 0
                {
                    assert!(
                        result["diagnostics"]["messages"]
                            .to_string()
                            .contains("fake 1"),
                        "{result}"
                    );
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "working check never delivered: {result}"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert_eq!(bridge_publications(&fixture), 1);
        }
        actor.call(&fixture, "ide.stop", json!({})).await;
        actor.mcp.close().await;
        daemon.kill().await.unwrap();
        daemon.wait().await.unwrap();
        for server in bridge_servers {
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
        }
    }
}

/// Calls one shipping Claude tool with exact pre/post pairing and retains its rendered body.
/// The normal fixture decoder omits edit diagnostic text, so this returns both parsed fields
/// and the independently checked Claude envelope for assertions without changing that decoder.
async fn claude_call_with_text(
    actor: &mut ProductActor,
    fixture: &ProductFixture,
    name: &str,
    arguments: Value,
) -> (Value, String) {
    actor.next += 1;
    let call = format!("call-{}", actor.next);
    actor.claude_lifecycle(fixture, "PreToolUse", &call).await;
    let reply = actor
        .mcp
        .exchange(
            json!({"jsonrpc":"2.0","id":actor.next,"method":"tools/call","params":{
                "name":name,"arguments":arguments,"_meta":{"claudecode/toolUseId":call}
            }}),
        )
        .await;
    let post = actor
        .claude_lifecycle_output(fixture, "PostToolUse", &call)
        .await;
    assert!(post.status.success() && post.stderr.is_empty());
    let text = assert_claude_envelope(&reply).to_owned();
    (claude_fields(name, &text), text)
}

/// A first transient failed/timed-out check never suppresses later Claude IDE edit checks.
/// The controlled fake cargo runs under the real confined runner. A retry-mode flag in its
/// admitted Cargo home (outside the worktree/fingerprint) only changes what a subsequent
/// invocation returns; no native writer hook or explicit problem read schedules that check.
#[tokio::test]
async fn claude_edit_reinvokes_checks_after_first_fatal_or_timeout() {
    for timeout in [false, true] {
        let fixture = ProductFixture::new(json!([]));
        let home = enable_fake_rust_checks(&fixture, &fixture.base);
        let cargo = home.join(".rustup/toolchains/fake/bin/cargo");
        let normal = std::fs::read_to_string(&cargo).unwrap();
        let first = if timeout {
            "/bin/sleep 30"
        } else {
            "printf 'first transient check failure\\n' >&2; exit 101"
        };
        std::fs::write(&cargo, format!(r#"#!/bin/sh
if [ ! -f "$CARGO_HOME/retry-enabled" ]; then
  {first}
fi
printf '%s\n' '{{"reason":"compiler-message","package_id":"fixture","message":{{"level":"warning","message":"retry checker actually reinvoked","code":null,"spans":[{{"file_name":"src/lib.rs","is_primary":true,"line_start":1,"column_start":1}}]}}}}'
{normal}"#)).unwrap();
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
        config["project_checks"]["check_timeout_s"] = json!(10);
        std::fs::write(&fixture.config, config.to_string()).unwrap();
        let mut daemon = fixture.daemon_with_home(Some(&home)).await;
        let (mut actor, _) = eyes_claude_actor(&fixture, "transient-check-retry").await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let (problems, _) = actor
                .call_claude_with_post(&fixture, "ide.inspect", json!({"kind":"problems"}))
                .await;
            assert_eq!(problems["kind"], "inspect", "{problems}");
            let text = problems["text"].as_str().unwrap();
            if text.contains(if timeout {
                "check timed out"
            } else {
                "check failed"
            }) {
                if !timeout {
                    assert!(text.contains("first transient check failure"), "{text}");
                }
                println!(
                    "initial {}: {text}",
                    if timeout { "Timeout" } else { "Fatal" }
                );
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "initial transient attempt did not settle: {text}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        std::fs::write(home.join(".cargo/retry-enabled"), "go").unwrap();
        let (read, _) = actor
            .call_claude_with_post(
                &fixture,
                "ide.read",
                json!({"items":[{"path":"src/lib.rs"}]}),
            )
            .await;
        let (read, _) = actor.settle_claude(&fixture, read).await;
        assert_eq!(read["kind"], "read", "{read}");
        let (mut edit, mut text) = claude_call_with_text(
            &mut actor,
            &fixture,
            "ide.edit",
            json!({
                "operation_id":"retry-after-transient", "path":"src/lib.rs",
                "changes":[{"old":"{ 7 }","new":"{ 8 }"}]
            }),
        )
        .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while edit["state"] == "pending" || text.contains("diagnostics: pending") {
            let reference = edit["result"]["source_ref"]
                .as_str()
                .or_else(|| edit["detail_ref"].as_str())
                .unwrap()
                .to_owned();
            (edit, text) = claude_call_with_text(
                &mut actor,
                &fixture,
                "ide.inspect",
                json!({"detail_ref":reference}),
            )
            .await;
            assert!(
                tokio::time::Instant::now() < deadline,
                "retry check did not settle: {edit}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(edit["result"]["outcome"], "replaced", "{edit}: {text}");
        assert!(text.contains("diagnostics: current_reported"), "{text}");
        assert!(text.contains("retry checker actually reinvoked"), "{text}");
        println!(
            "after {}: successful Claude ide.edit; checker reinvoked",
            if timeout { "Timeout" } else { "Fatal" }
        );
        let source = std::fs::read_to_string(fixture.root.join("src/lib.rs")).unwrap();
        assert!(
            source
                .split_whitespace()
                .collect::<String>()
                .contains("pubfnvalue()->i32{8}"),
            "{source}"
        );
        actor
            .call_claude_with_post(&fixture, "ide.stop", json!({}))
            .await;
        actor.mcp.close().await;
        daemon.kill().await.unwrap();
        daemon.wait().await.unwrap();
    }
}

/// Runs 2,000 real Cargo tests with one failure through shipping MCP. Completion and saved
/// results stay below 4 KiB, while explicit output handles deliver each page only once.
#[tokio::test]
async fn cargo_2000_tests_keep_results_compact_and_log_pages_explicit() {
    let fixture = ProductFixture::new(json!([]));
    let mut source = String::new();
    for index in 0..2000 {
        let body = if index == 1234 {
            "panic!(\"result-economy failure message\");"
        } else {
            ""
        };
        source.push_str(&format!(
            "/// Product fixture test {index}.\n#[test] fn case_{index:04}_with_a_long_name_to_exercise_real_cargo_log_pagination() {{ {body} }}\n"
        ));
    }
    std::fs::write(fixture.root.join("src/lib.rs"), source).unwrap();
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "compact-cargo-results").await;
    let started = actor.call(&fixture, "ide.start", json!({})).await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let run = actor.call(&fixture, "ide.test", json!({
        "command":[std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()), "test", "--offline", "--lib", "--", "--test-threads=1"],
        "budget_s":120
    })).await;
    let reference = run["detail_ref"]
        .as_str()
        .expect("run reference")
        .to_owned();
    let mut result = actor.settle(&fixture, run).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while !result["text"]
        .as_str()
        .unwrap_or_default()
        .contains("1999 passed, 1 failed")
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "Cargo never settled: {result}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        result = actor.call(&fixture, "ide.test", json!({"result":1})).await;
    }
    let text = result["text"].as_str().unwrap();
    println!("2,000-test completion: {} bytes", text.len());
    assert!(text.len() < 4096, "{} completion bytes", text.len());
    assert!(
        text.contains("FAIL case_1234") && text.contains("result-economy failure message"),
        "{text}"
    );
    assert!(text.contains("exit 101"), "{text}");
    assert!(!text.contains("test case_0000"), "full log leaked: {text}");
    for _ in 0..2 {
        let saved = actor
            .call(&fixture, "ide.test", json!({"detail_ref":reference}))
            .await;
        let text = saved["text"].as_str().unwrap();
        assert!(
            text.contains("1999 passed, 1 failed")
                && text.contains("result-economy failure message"),
            "{saved}"
        );
        assert!(text.len() < 4096 && !text.contains("test case_0000"));
    }
    let first_ref = format!("{reference}/output/1");
    let mut page = actor
        .call(&fixture, "ide.test", json!({"detail_ref":first_ref}))
        .await;
    assert!(
        page["text"].as_str().unwrap().contains("test case_0000"),
        "{page}"
    );
    assert_eq!(page["continuation"], true, "Cargo output should span pages");
    let repeated = actor
        .call(&fixture, "ide.test", json!({"detail_ref":first_ref}))
        .await;
    assert_eq!(
        repeated["text"].as_str().unwrap().lines().count(),
        1,
        "{repeated}"
    );
    assert!(
        repeated["text"]
            .as_str()
            .unwrap()
            .contains("already delivered"),
        "{repeated}"
    );
    let mut last_ref = first_ref;
    while page["continuation"] == true {
        last_ref = page["detail_ref"].as_str().unwrap().to_owned();
        page = actor
            .call(&fixture, "ide.test", json!({"detail_ref":last_ref}))
            .await;
    }
    assert!(
        page["text"]
            .as_str()
            .unwrap()
            .contains("test result: FAILED. 1999 passed; 1 failed"),
        "{page}"
    );
    let repeated = actor
        .call(&fixture, "ide.test", json!({"detail_ref":last_ref}))
        .await;
    assert_eq!(
        repeated["text"].as_str().unwrap().lines().count(),
        1,
        "{repeated}"
    );
    assert!(
        repeated["text"]
            .as_str()
            .unwrap()
            .contains("already delivered"),
        "{repeated}"
    );
    // The original saved receipt outlives the bounded four-run test-history registry.
    for _ in 0..4 {
        let run = actor
            .call(
                &fixture,
                "ide.test",
                json!({"command":["/bin/sh","-c",":"]}),
            )
            .await;
        let run = actor.settle(&fixture, run).await;
        assert!(run["text"].as_str().unwrap().contains("exit 0"), "{run}");
    }
    let saved = actor
        .call(&fixture, "ide.test", json!({"detail_ref":reference}))
        .await;
    assert!(
        saved["text"]
            .as_str()
            .unwrap()
            .contains("result-economy failure message"),
        "{saved}"
    );
    actor.call(&fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// The assembled facade/worker output contract restores consumed bodies after context loss
/// without rerunning the side effect or advancing ordinary pages.
/// This requires the M-038 output schema together with M-037 retained output implementation.
#[tokio::test]
async fn explicit_output_body_recovery_is_readonly_through_the_assembled_mcp() {
    let fixture = ProductFixture::new(json!([]));
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "output-recovery").await;
    let start = actor.call(&fixture, "ide.start", json!({})).await;
    assert_eq!(actor.settle(&fixture, start).await["kind"], "activation");
    let run = actor.call(&fixture, "ide.test", json!({"command":["/bin/sh","-c","printf invoked >> output-run-count; i=0; while [ $i -lt 2000 ]; do printf 'retained output recovery log line with enough bytes for many pages\\n'; i=$((i+1)); done"]})).await;
    let reference = run["detail_ref"].as_str().unwrap().to_owned();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let result = actor.call(&fixture, "ide.test", json!({"result":1})).await;
        if result["text"]
            .as_str()
            .unwrap_or_default()
            .contains("exit 0")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "run did not settle: {result}"
        );
        tokio::task::yield_now().await;
    }
    let first_ref = format!("{reference}/output/1");
    let mut page = actor
        .call(&fixture, "ide.test", json!({"detail_ref":first_ref}))
        .await;
    let first = page.clone();
    assert_eq!(first["continuation"], true);
    let restored = actor
        .call(&fixture, "ide.test", json!({"output":first_ref}))
        .await;
    assert_eq!(restored, first);
    let mut last_ref = first_ref.clone();
    while page["continuation"] == true {
        last_ref = page["detail_ref"].as_str().unwrap().to_owned();
        page = actor
            .call(&fixture, "ide.test", json!({"detail_ref":last_ref}))
            .await;
    }
    let last = page;
    for (reference, expected) in [(&first_ref, &first), (&last_ref, &last)] {
        let duplicate = actor
            .call(&fixture, "ide.test", json!({"detail_ref":reference}))
            .await;
        assert!(
            duplicate["text"]
                .as_str()
                .unwrap()
                .contains("already delivered")
        );
        let restored = actor
            .call(&fixture, "ide.test", json!({"output":reference}))
            .await;
        assert_eq!(&restored, expected);
    }
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("output-run-count")).unwrap(),
        "invoked"
    );
    let saved = actor
        .call(&fixture, "ide.test", json!({"detail_ref":reference}))
        .await;
    assert!(saved["text"].as_str().unwrap().contains("exit 0"));
    assert!(
        !saved["text"]
            .as_str()
            .unwrap()
            .contains("retained output recovery log")
    );
    actor.call(&fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}
