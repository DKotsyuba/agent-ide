//! One public wire boundary control spanning strict input and immutable output.
use bridge_protocol::*;
use serde_json::{Value, json};

/// Reusable accepted fixture set; contains only intentionally synthetic credentials.
fn fixtures() -> Vec<Request> {
    serde_json::from_str::<Value>(include_str!("wire-v1.json")).unwrap()["requests"]
        .as_array()
        .unwrap()
        .clone()
        .into_iter()
        .map(|v| decode_request(&serde_json::to_vec(&v).unwrap()).unwrap())
        .collect()
}
/// Exact normalized custody acknowledgement for a supplied synthetic publication.
fn receipt(p: &PublishParams) -> EventReceipt {
    let address = match &p.destination {
        Destination::Address { address } => NativeAddress::parse(address).unwrap().canonical(),
        _ => "local/codex/store/session".into(),
    };
    EventReceipt {
        producer: "synthetic-producer".into(),
        event_id: p.event_id.clone(),
        intent_hash: publication_intent_fingerprint(p).unwrap(),
        envelope_hash: "a".repeat(64),
        accepted_at_ms: 10,
        acceptance_seq: 1,
        destination: FrozenDestination {
            address,
            route_key: None,
            caller_scope: None,
            destination_scope: None,
        },
    }
}
/// The decisive public control rejects genuine malformed/altered immutable replies.
#[test]
fn public_wire_boundary() {
    let all = fixtures();
    let fixture: Value = serde_json::from_str(include_str!("wire-v1.json")).unwrap();
    for (request, response) in all.iter().zip(fixture["responses"].as_array().unwrap()) {
        let bytes = serde_json::to_vec(response).unwrap();
        let response = decode_response(&bytes, request).unwrap();
        assert_eq!(
            decode_response(&encode_response(&response).unwrap(), request).unwrap(),
            response
        );
    }
    let mut bounded = all[1].clone();
    if let Operation::RegisterCall(p) = &mut bounded.operation {
        p.max_payload_bytes = Some(100);
        p.allowed_kinds = Some(vec!["only.kind".into()]);
    }
    let forged = serde_json::to_vec(&fixture["responses"][1]).unwrap();
    assert!(
        decode_response(&forged, &bounded).is_err(),
        "broader registration constraints accepted"
    );
    for request in &all {
        let bytes = encode_request(request).unwrap();
        assert_eq!(decode_request(&bytes).unwrap(), *request);
        assert!(!format!("{request:?}").contains("fixture-secret"));
    }
    let mut register = match all[1].operation.clone() {
        Operation::RegisterCall(p) => p,
        _ => unreachable!(),
    };
    let base = registration_request_fingerprint(&register).unwrap();
    register.evidence = None;
    register.allowed_modes = Some(vec![Mode::Message]);
    assert_eq!(registration_request_fingerprint(&register).unwrap(), base);
    register.allowed_kinds = Some(vec!["*".into()]);
    assert_ne!(registration_request_fingerprint(&register).unwrap(), base);
    register.allowed_modes = Some(vec![Mode::Message, Mode::Inject]);
    let a = registration_request_fingerprint(&register).unwrap();
    register.allowed_modes = Some(vec![Mode::Inject, Mode::Message]);
    assert_eq!(registration_request_fingerprint(&register).unwrap(), a);
    let mut publication = match all[2].operation.clone() {
        Operation::Publish(p) => p,
        _ => unreachable!(),
    };
    let request = &all[2];
    let good = Response::ok(
        &request.request_id,
        OperationResult::Publish(PublishResult {
            receipt: receipt(&publication),
            replayed: false,
        }),
    );
    // R1: top-level key presence is strict even when the extra value is null.
    let safe_error = Response::error(
        &request.request_id,
        ApiError {
            class: ErrorClass::Auth,
            reason: "refused".into(),
            retry_hint: RetryHint::Never,
            effect: Some(MutationEffect::NotCommitted),
            reconciliation: request.reconciliation(),
            detail: "safe".into(),
        },
    );
    for (response, extra_key) in [(&good, "error"), (&safe_error, "result")] {
        let mut malformed = serde_json::to_value(response).unwrap();
        malformed[extra_key] = Value::Null;
        assert!(
            serde_json::from_value::<Response>(malformed.clone()).is_err(),
            "explicit-null extra response key accepted"
        );
        assert!(decode_response(&serde_json::to_vec(&malformed).unwrap(), request).is_err());
    }
    for key in ["result", "error"] {
        let mut malformed =
            json!({"api_version":1,"request_id":request.request_id,"success":key=="result"});
        malformed[key] = Value::Null;
        assert!(serde_json::from_value::<Response>(malformed).is_err());
    }
    // R2: all encoder envelopes and nested DTOs validate before serialization.
    let mut invalid_receipt = good.clone();
    if let Some(OperationResult::Publish(p)) = &mut invalid_receipt.result {
        p.receipt.acceptance_seq = 0;
    }
    assert!(
        encode_response(&invalid_receipt).is_err(),
        "invalid receipt encoded"
    );
    let mut invalid_version = good.clone();
    invalid_version.api_version = 2;
    let mut invalid_id = good.clone();
    invalid_id.request_id.clear();
    let mut invalid_shape = good.clone();
    invalid_shape.error = safe_error.error.clone();
    let mut invalid_error = safe_error.clone();
    if let Some(e) = &mut invalid_error.error {
        e.effect = Some(MutationEffect::Unknown);
        e.reconciliation = None;
    }
    for invalid in [invalid_version, invalid_id, invalid_shape, invalid_error] {
        assert!(encode_response(&invalid).is_err());
    }
    let error_bytes = encode_response(&safe_error).unwrap();
    assert_eq!(decode_response(&error_bytes, request).unwrap(), safe_error);
    let bytes = encode_response(&good).unwrap();
    assert_eq!(decode_response(&bytes, request).unwrap(), good);
    publication.payload = Payload::new("changed body").unwrap();
    let changed = Request::new(
        &request.request_id,
        Secret::new("fixture-secret"),
        Operation::Publish(publication),
    )
    .unwrap();
    assert!(decode_response(&bytes, &changed).is_err());
    let mut wrong = good.clone();
    wrong.request_id = "other-call".into();
    assert!(wrong.validate_for(request).is_err());
    let bad = [
        r#"{"api_version":1,"api_version":1,"request_id":"x","credential":"s","op":"capabilities","params":{}}"#,
        r#"{"api_version":1,"request_id":"x","credential":"s","op":"capabilities","params":{"producer":"forged"}}"#,
        r#"{"api_version":2,"request_id":"x","credential":"s","op":"capabilities","params":{}}"#,
        r#"{"api_version":1,"request_id":"x","credential":"s","op":"register_call","params":{"operation_key":"o","evidence":{"type":"native","host":"codex","metadata":{"a":1,"a":2}}}}"#,
    ];
    for raw in bad {
        assert!(
            decode_request(raw.as_bytes()).is_err(),
            "invalid frame accepted"
        );
    }
    let mut extra = serde_json::to_value(&good).unwrap();
    extra["error"] = json!({"class":"auth","reason":"refused","retry_hint":"never","effect":null,"reconciliation":null,"detail":"safe"});
    assert!(decode_response(&serde_json::to_vec(&extra).unwrap(), request).is_err());
    let parsed = NativeAddress::parse("h%2fx/codex/s%25t/thread%2fchild").unwrap();
    assert_eq!(parsed.canonical(), "h%2Fx/codex/s%25t/thread%2Fchild");
    assert!(NativeAddress::parse("h/codex/s/x%GG").is_err());
    assert!(Payload::new("x\u{85}").is_err());
    let mut oversized = match all[2].operation.clone() {
        Operation::Publish(p) => p,
        _ => unreachable!(),
    };
    oversized.kind = "k".repeat(65);
    assert!(oversized.validate().is_err());
    let evidence = format!("{:?}", all[1]);
    assert!(!evidence.contains("native-thread"));
}
