//! One real Unix boundary control for retries, uncertainty, deadlines and redaction.
use bridge_sdk::bridge_protocol::*;
use bridge_sdk::{Client, ClientError, Config, TransportOutcome, extract_native_evidence};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

/// Synthetic publication fixture, deliberately unrelated to any consumer product.
fn publication() -> PublishParams {
    PublishParams {
        destination: Destination::Address {
            address: "local/codex/store/session".into(),
        },
        event_id: "independent-event".into(),
        kind: "independent.kind".into(),
        payload: Payload::new("never-log-this-body").unwrap(),
        mode: Mode::Message,
        policy: Policy::KeepAll,
        metadata: Metadata::new(),
        expires_at_ms: None,
        delivery_scope_key: None,
    }
}
/// Correlated contract response; this substitute owns no store/native business logic.
fn acknowledge(request: &Request) -> Vec<u8> {
    let p = match &request.operation {
        Operation::Publish(p) => p,
        _ => panic!("fixture expected publication"),
    };
    let receipt = EventReceipt {
        producer: "independent-producer".into(),
        event_id: p.event_id.clone(),
        intent_hash: publication_intent_fingerprint(p).unwrap(),
        envelope_hash: "a".repeat(64),
        accepted_at_ms: 10,
        acceptance_seq: 1,
        destination: FrozenDestination {
            address: "local/codex/store/session".into(),
            route_key: None,
            caller_scope: None,
            destination_scope: None,
        },
    };
    let mut bytes = encode_response(&Response::ok(
        &request.request_id,
        OperationResult::Publish(PublishResult {
            receipt,
            replayed: true,
        }),
    ))
    .unwrap();
    bytes.push(b'\n');
    bytes
}
/// Opens only a task-private socket beneath the explicitly configured TMPDIR.
fn path(name: &str) -> PathBuf {
    let base = PathBuf::from(std::env::var_os("TMPDIR").expect("explicit task TMPDIR required"));
    assert!(base.starts_with("/Users/pluto/projects/tmp/agent-bridge"));
    base.join(format!("sdk-{}-{name}.sock", std::process::id()))
}
/// One reusable public control exercises real streams, not an artificial transport trait.
#[test]
fn public_sdk_boundary() {
    let socket = path("retry");
    let listener = UnixListener::bind(&socket).unwrap();
    let worker = thread::spawn(move || {
        let mut first = String::new();
        let (stream, _) = listener.accept().unwrap();
        BufReader::new(stream).read_line(&mut first).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let mut second = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut second)
            .unwrap();
        assert_eq!(first, second, "retry changed immutable request");
        let request = decode_request(second.trim_end().as_bytes()).unwrap();
        stream.write_all(&acknowledge(&request)).unwrap();
    });
    let config = Config::new(
        &socket,
        &socket,
        Secret::new("never-log-this-credential"),
        Duration::from_secs(1),
    )
    .unwrap();
    assert!(!format!("{config:?}").contains("never-log-this-credential"));
    let client = Client::new(config).unwrap();
    let received = client.publish("stable-request", publication()).unwrap();
    assert!(received.replayed);
    worker.join().unwrap();
    fs::remove_file(&socket).unwrap();

    let socket = path("lost");
    let listener = UnixListener::bind(&socket).unwrap();
    let owned = socket.clone();
    let worker = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut text = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut text)
            .unwrap();
        drop(listener);
        fs::remove_file(owned).unwrap();
        drop(stream);
    });
    let client = Client::new(
        Config::new(
            &socket,
            &socket,
            Secret::new("never-log-this-credential"),
            Duration::from_secs(1),
        )
        .unwrap(),
    )
    .unwrap();
    let error = client.publish("stable-request", publication()).unwrap_err();
    worker.join().unwrap();
    assert!(
        matches!(
            error,
            ClientError::Transport {
                outcome: TransportOutcome::OutcomeUnknown,
                reconciliation: Some(ReconciliationKey::Event { .. }),
                ..
            }
        ),
        "earlier unknown masked by later connect failure"
    );
    let diagnostic = format!("{error:?}");
    assert!(!diagnostic.contains("never-log-this-credential"));
    assert!(!diagnostic.contains("never-log-this-body"));

    let socket = path("reject");
    let listener = UnixListener::bind(&socket).unwrap();
    let worker = thread::spawn(move || {
        let (first, _) = listener.accept().unwrap();
        let mut frame = String::new();
        BufReader::new(first).read_line(&mut frame).unwrap();
        let (mut second, _) = listener.accept().unwrap();
        let mut replay = String::new();
        BufReader::new(second.try_clone().unwrap())
            .read_line(&mut replay)
            .unwrap();
        assert_eq!(frame, replay);
        let request = decode_request(replay.trim_end().as_bytes()).unwrap();
        let response = Response::error(
            &request.request_id,
            ApiError {
                class: ErrorClass::Auth,
                reason: "revoked".into(),
                retry_hint: RetryHint::Never,
                effect: Some(MutationEffect::NotCommitted),
                reconciliation: request.reconciliation(),
                detail: "retry credential rejected before original lookup".into(),
            },
        );
        let mut bytes = encode_response(&response).unwrap();
        bytes.push(b'\n');
        second.write_all(&bytes).unwrap();
    });
    let client = Client::new(
        Config::new(
            &socket,
            &socket,
            Secret::new("credential"),
            Duration::from_secs(1),
        )
        .unwrap(),
    )
    .unwrap();
    let error = client.publish("stable-request", publication()).unwrap_err();
    worker.join().unwrap();
    fs::remove_file(&socket).unwrap();
    assert!(
        matches!(
            error,
            ClientError::Transport {
                outcome: TransportOutcome::OutcomeUnknown,
                reconciliation: Some(ReconciliationKey::Event { .. }),
                ..
            }
        ),
        "later request refusal erased earlier mutation uncertainty"
    );

    let socket = path("slow");
    let listener = UnixListener::bind(&socket).unwrap();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut text = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut text)
            .unwrap();
        for _ in 0..30 {
            if stream.write_all(b" ").is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    let mut config = Config::new(
        &socket,
        &socket,
        Secret::new("credential"),
        Duration::from_millis(70),
    )
    .unwrap();
    config.reconnect_once = false;
    let client = Client::new(config).unwrap();
    let start = Instant::now();
    let error = client.publish("stable-request", publication()).unwrap_err();
    let elapsed = start.elapsed();
    assert!(matches!(
        error,
        ClientError::Transport {
            outcome: TransportOutcome::OutcomeUnknown,
            ..
        }
    ));
    assert!(
        elapsed < Duration::from_millis(300),
        "partial traffic renewed deadline"
    );
    worker.join().unwrap();
    fs::remove_file(&socket).unwrap();

    let client = Client::new(
        Config::new(
            &socket,
            &socket,
            Secret::new("credential"),
            Duration::from_millis(50),
        )
        .unwrap(),
    )
    .unwrap();
    let error = client.publish("stable-request", publication()).unwrap_err();
    assert!(matches!(
        error,
        ClientError::Transport {
            outcome: TransportOutcome::NotSent,
            ..
        }
    ));
    let native = serde_json::json!({"threadId":"native-thread","callId":"native-call","x-codex-turn-metadata":{}});
    assert!(extract_native_evidence(Host::Codex, &native).is_ok());
    let mixed = serde_json::json!({"threadId":"native-thread","callId":"native-call","x-codex-turn-metadata":{},"claudecode/toolUseId":"other"});
    assert!(extract_native_evidence(Host::Codex, &mixed).is_err());
    assert!(
        extract_native_evidence(
            Host::Codex,
            &serde_json::json!({"arguments":{"threadId":"native-thread"}})
        )
        .is_err()
    );
}
