//! Bounded packet serialization and the offline early-context prototype.
//! Callers retain source authority/freshness; this unit does no I/O or delivery accounting.

use serde_json::Value;

/// Serializes actual JSON bytes under the existing 8 KiB state ceiling.
pub(super) fn encode(state: &Value) -> Result<String, &'static str> {
    let encoded = serde_json::to_string(state).map_err(|_| "state_encoding")?;
    if encoded.len() > 8 * 1024 {
        return Err("state_budget");
    }
    Ok(encoded)
}

/// First-slice executable prototype, deliberately not admitted by the runtime yet.
/// Promotion needs the real authorized activation and source-backed outbox integration.
#[cfg(test)]
mod prototype {
    use super::*;
    use crate::assistance::worker::support::client;
    use serde::Serialize;
    use std::collections::BTreeMap;
    use std::ops::Range;
    use std::path::{Path, PathBuf};

    /// Identity supplied by an already-authorized activation, never a grant.
    #[derive(Clone, PartialEq, Eq, Serialize)]
    struct Frame {
        binding: String,
        generation: u64,
        activation: String,
        epoch: u64,
    }

    /// Exact excerpt of caller-verified bytes; its digest covers the complete source.
    #[derive(Clone, Serialize)]
    struct Window {
        path: PathBuf,
        version: String,
        start_byte: usize,
        end_byte: usize,
        text: String,
        relation: &'static str,
        source_bytes: usize,
    }

    impl Window {
        /// Validates UTF-8 boundaries without reading a file or inventing source authority.
        fn capture(path: &Path, source: &str, range: Range<usize>) -> Result<Self, &'static str> {
            if !crate::workspace::observation::valid_relative_path(path) {
                return Err("source_path");
            }
            let text = source.get(range.clone()).ok_or("source_range")?;
            if source.len() > 1024 * 1024 || text.len() > 2048 {
                return Err("source_budget");
            }
            Ok(Self {
                path: path.to_owned(),
                version: blake3::hash(source.as_bytes()).to_hex().to_string(),
                start_byte: range.start,
                end_byte: range.end,
                text: text.to_owned(),
                relation: "observed_only; not proof of test coverage",
                source_bytes: source.len(),
            })
        }
    }

    /// Closed capabilities reference only the caller's bounded catalog and verified windows.
    struct Packet {
        frame: Frame,
        catalog: Vec<PathBuf>,
        windows: Vec<Window>,
        encoded: String,
        candidates: Vec<client::Candidate>,
    }

    /// Selected utility operation; neither variant grants a project write.
    enum Effect {
        /// Caller must resolve this catalog position through its normal source guards.
        Read(usize),
        /// Concrete evidence, not a declaration that the task or contract is correct.
        Bundle(Value),
    }

    impl Packet {
        /// Builds a deterministic metadata page and de-duplicated evidence without ambient reads.
        fn build(
            frame: Frame,
            task: &str,
            catalog: &[PathBuf],
            windows: &[Window],
            run_facts: Value,
        ) -> Result<Self, &'static str> {
            if task.trim().is_empty() || task.len() > 1024 || catalog.len() > 32 {
                return Err("packet_input");
            }
            if catalog
                .iter()
                .any(|p| !crate::workspace::observation::valid_relative_path(p))
            {
                return Err("source_path");
            }
            let mut catalog = catalog.to_vec();
            catalog.sort();
            catalog.dedup();
            let omitted = catalog.len().saturating_sub(10);
            catalog.truncate(10);
            let mut unique = BTreeMap::new();
            let mut charged = BTreeMap::new();
            for window in windows {
                if !catalog.contains(&window.path) {
                    return Err("window_not_observed");
                }
                charged.insert((&window.path, &window.version), window.source_bytes);
                unique.insert(
                    (
                        &window.path,
                        &window.version,
                        window.start_byte,
                        window.end_byte,
                    ),
                    window.clone(),
                );
            }
            if unique.len() > 8 || charged.values().sum::<usize>() > 2 * 1024 * 1024 {
                return Err("read_budget");
            }
            let windows: Vec<_> = unique.into_values().collect();
            let state = serde_json::json!({
                "phase": if windows.is_empty() { "cold" } else { "context" },
                "task": task, "authority": frame, "catalog": catalog,
                "catalog_omitted": omitted, "evidence": windows,
                "run_facts": run_facts,
                "gaps": ["Intended behavior unresolved; observations are not a complete project audit."],
                "main_delivery": "not_recorded"
            });
            let encoded = encode(&state)?;
            let mut candidates = vec![client::Candidate {
                id: "a0".into(),
                description: "no_action: insufficient useful evidence".into(),
                auto_test: false,
            }];
            if !windows.is_empty() {
                candidates.push(client::Candidate {
                    id: "bundle".into(),
                    description: "publish the observed code, run facts and unresolved contract to the coding agent".into(),
                    auto_test: false,
                });
            }
            candidates.extend(
                catalog
                    .iter()
                    .enumerate()
                    .map(|(index, path)| client::Candidate {
                        id: format!("f{index}"),
                        description: format!(
                            "read authorized catalog candidate {path:?}; relevance not established"
                        ),
                        auto_test: false,
                    }),
            );
            Ok(Self {
                frame,
                catalog,
                windows,
                encoded,
                candidates,
            })
        }

        /// Accepts the existing Choice result only against a fresh activation and source bytes.
        fn select(
            &self,
            selection: &client::Selection,
            current: &Frame,
            sources: &[(&Path, &str)],
        ) -> Result<Option<Effect>, &'static str> {
            if current != &self.frame {
                return Err("stale_activation");
            }
            if !(0.5..=1.0).contains(&selection.confidence) {
                return Err("low_confidence");
            }
            if !self
                .candidates
                .iter()
                .any(|c| c.id == selection.selected_id)
            {
                return Err("unknown_id");
            }
            if selection.selected_id == "a0" {
                return Ok(None);
            }
            for window in &self.windows {
                let text = sources
                    .iter()
                    .find(|(path, _)| *path == window.path)
                    .map(|(_, text)| *text)
                    .ok_or("source_unavailable")?;
                if blake3::hash(text.as_bytes()).to_hex().as_str() != window.version {
                    return Err("stale_source");
                }
            }
            if selection.selected_id == "bundle" {
                let mut bundle: Value =
                    serde_json::from_str(&self.encoded).map_err(|_| "state_encoding")?;
                bundle["action"] = serde_json::json!("review_observed_context");
                return Ok(Some(Effect::Bundle(bundle)));
            }
            let index = self
                .candidates
                .iter()
                .find(|c| c.id == selection.selected_id)
                .and_then(|c| c.id.strip_prefix('f'))
                .and_then(|id| id.parse::<usize>().ok())
                .ok_or("unknown_id")?;
            if index >= self.catalog.len() {
                return Err("unknown_id");
            }
            Ok(Some(Effect::Read(index)))
        }
    }

    /// Scripted response uses the real transport result type and performs no inference.
    fn selected(id: &str, confidence: f64) -> client::Selection {
        client::Selection {
            selected_id: id.into(),
            confidence,
            auto_test_probability: None,
            request: Value::Null,
            response: Value::Null,
            usage: None,
            attempt: client::Attempt::default(),
        }
    }

    /// Synthetic activation identity carries no host credential or native delivery receipt.
    fn frame() -> Frame {
        Frame {
            binding: "fixture".into(),
            generation: 1,
            activation: "cold".into(),
            epoch: 1,
        }
    }

    #[test]
    /// Cold metadata selection becomes a code-bearing unresolved bundle before any edit.
    fn cold_search_read_selection_then_concrete_bundle() {
        let path = PathBuf::from("src/request.ts");
        let text = "const cachedBody = bodyCache[key];\nreturn (bodyCache[key] = raw[key]());\n";
        let catalog = vec![path.clone(), PathBuf::from("src/request.test.ts")];
        let cold = Packet::build(
            frame(),
            "Restore public behavior.",
            &catalog,
            &[],
            Value::Null,
        )
        .unwrap();
        assert!(cold.windows.is_empty());
        assert!(matches!(
            cold.select(&selected("f1", 0.7), &frame(), &[]).unwrap(),
            Some(Effect::Read(1))
        ));
        let source = Window::capture(&path, text, 0..text.len()).unwrap();
        let assertion_path = PathBuf::from("src/request.test.ts");
        let assertion = "expect(async () => await req.text()).not.toThrow();";
        let test_window = Window::capture(&assertion_path, assertion, 0..assertion.len()).unwrap();
        let facts = serde_json::json!({
            "status":"failed", "frame":"src/request.ts:2",
            "message":"Body is unusable: Body has already been read"
        });
        let packet = Packet::build(
            frame(),
            "Restore public behavior.",
            &catalog,
            &[source.clone(), source, test_window],
            facts,
        )
        .unwrap();
        assert_eq!(
            packet.windows.len(),
            2,
            "one verified window must not spend twice the state"
        );
        let Some(Effect::Bundle(bundle)) = packet
            .select(
                &selected("bundle", 0.7),
                &frame(),
                &[(&path, text), (&assertion_path, assertion)],
            )
            .unwrap()
        else {
            panic!("expected concrete bundle");
        };
        let evidence = bundle["evidence"].as_array().unwrap();
        assert!(evidence.iter().any(|item| item["text"] == text));
        assert!(evidence.iter().any(|item| item["text"] == assertion));
        assert_eq!(bundle["main_delivery"], "not_recorded");
        assert!(bundle["gaps"][0].as_str().unwrap().contains("unresolved"));
        assert_eq!(bundle["run_facts"]["status"], "failed");
        let again = Packet::build(
            frame(),
            "Restore public behavior.",
            &catalog,
            &packet.windows,
            bundle["run_facts"].clone(),
        )
        .unwrap();
        assert_eq!(packet.encoded, again.encoded);
    }

    #[test]
    /// Unsafe selections, changed files and native generation changes produce no bundle.
    fn selection_refusals_and_out_of_band_freshness() {
        let path = PathBuf::from("src/input.gamma");
        let text = "sym source\n";
        let window = Window::capture(&path, text, 0..text.len()).unwrap();
        let packet = Packet::build(
            frame(),
            "Review task.",
            std::slice::from_ref(&path),
            &[window],
            Value::Null,
        )
        .unwrap();
        for confidence in [f64::NAN, -1.0, 0.49, 1.1] {
            assert!(matches!(
                packet.select(&selected("bundle", confidence), &frame(), &[(&path, text)]),
                Err("low_confidence")
            ));
        }
        assert!(matches!(
            packet.select(&selected("unknown", 0.9), &frame(), &[(&path, text)]),
            Err("unknown_id")
        ));
        assert!(matches!(
            packet.select(
                &selected("bundle", 0.9),
                &frame(),
                &[(&path, "sym changed\n")]
            ),
            Err("stale_source")
        ));
        let mut newer = frame();
        newer.epoch += 1;
        assert!(matches!(
            packet.select(&selected("bundle", 0.9), &newer, &[(&path, text)]),
            Err("stale_activation")
        ));
        assert!(
            packet
                .select(&selected("a0", 0.9), &frame(), &[])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    /// Only explicitly observed catalog regions can enter a packet; omissions remain visible.
    fn provenance_canaries_and_explicit_catalog_omission() {
        let path = PathBuf::from("src/visible.gamma");
        let hidden = PathBuf::from("controls/hidden-canary.gamma");
        let window = Window::capture(&hidden, "PRIVATE-CANARY", 0..14).unwrap();
        assert!(matches!(
            Packet::build(frame(), "Task", &[path], &[window], Value::Null),
            Err("window_not_observed")
        ));
        assert!(matches!(
            Window::capture(Path::new("../oracle"), "x", 0..1),
            Err("source_path")
        ));
        let catalog: Vec<_> = (0..20)
            .map(|i| PathBuf::from(format!("src/{i:02}.gamma")))
            .collect();
        let packet = Packet::build(frame(), "Task", &catalog, &[], Value::Null).unwrap();
        let value: Value = serde_json::from_str(&packet.encoded).unwrap();
        assert_eq!(value["catalog_omitted"], 10);
        assert!(!packet.encoded.contains("PRIVATE-CANARY"));
        assert!(packet.candidates.len() <= 12);
    }

    #[test]
    /// Encoded UTF-8 JSON bytes, not character counts, determine the packet budget.
    fn utf8_and_serialized_budget_are_exact() {
        let path = PathBuf::from("src/unicode.gamma");
        assert!(matches!(
            Window::capture(&path, "ёж", 1..3),
            Err("source_range")
        ));
        let value = serde_json::json!({"text":"я".repeat(4096)});
        assert_eq!(encode(&value), Err("state_budget"));
        let text = "\"".repeat(2048);
        let window = Window::capture(&path, &text, 0..text.len()).unwrap();
        let packet = Packet::build(frame(), "Task", &[path], &[window], Value::Null).unwrap();
        assert!(packet.encoded.len() <= 8192);
    }
    #[tokio::test]
    #[ignore = "operator-only free Jev measurement; not an offline or automatic gate"]
    /// Records one real free-route selection from public observations, with no project writes.
    async fn manual_free_context_selection_measurement() {
        use super::super::{Budget, Trace, record_attempt, token_totals};
        use std::os::unix::fs::OpenOptionsExt;

        let root =
            std::path::PathBuf::from(std::env::var("AGENT_IDE_JEV_PACKET_PROBE_ROOT").unwrap());
        assert_eq!(root.canonicalize().unwrap(), root);
        let output =
            std::path::PathBuf::from(std::env::var("AGENT_IDE_JEV_PACKET_PROBE_OUT").unwrap());
        assert!(output.starts_with("/Users/pluto/projects/tmp/agent-ide"));
        assert_eq!(output.canonicalize().unwrap(), output);
        let key =
            std::path::PathBuf::from(std::env::var("AGENT_IDE_JEV_PACKET_PROBE_KEYFILE").unwrap());
        let client =
            client::JevClient::from_credential_file(&key, client::Route::OpenCode).unwrap();
        let paths = [
            PathBuf::from("src/request.ts"),
            PathBuf::from("src/request.test.ts"),
        ];
        let bodies: Vec<_> = paths
            .iter()
            .map(|path| {
                assert!(root.join(path).canonicalize().unwrap().starts_with(&root));
                std::fs::read_to_string(root.join(path)).unwrap()
            })
            .collect();
        let windows: Vec<_> = paths
            .iter()
            .zip(&bodies)
            .zip([(205, 240), (245, 275)])
            .map(|((path, text), (first, last))| {
                let start = text
                    .split_inclusive('\n')
                    .take(first - 1)
                    .map(str::len)
                    .sum();
                let end = text.split_inclusive('\n').take(last).map(str::len).sum();
                Window::capture(path, text, start..end).unwrap()
            })
            .collect();
        let packet = Packet::build(frame(),
            "Restore this project to a reliable, usable state. Preserve its intended public functionality and compatibility.",
            &paths, &windows, serde_json::json!({
                "status":"failed", "provenance":"previously observed public test output; not evaluator controls",
                "message":"TypeError: Body is unusable: Body has already been read",
                "frame":"src/request.ts:226:38",
                "intended_behavior":"unresolved"
            })).unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(output.join("probe.jsonl"))
            .unwrap();
        let mut trace = Trace(file);
        let mut budget = Budget {
            calls: 1,
            ..Budget::default()
        };
        trace.record(serde_json::json!({
            "event":"utility_probe_inputs", "measurement":"operator-only packet prototype, not IDE handoff",
            "state":packet.encoded, "options":packet.candidates.iter()
                .map(|c|serde_json::json!({"id":c.id,"description":c.description,"auto_test":c.auto_test})).collect::<Vec<_>>()
        })).unwrap();
        let selection =
            client
                .decide(&packet.encoded, &packet.candidates, |request| {
                    let bytes = serde_json::to_vec(request).map_err(|_| ())?.len();
                    trace.record(serde_json::json!({
                "event":"request_pending","decision":1,"route":"opencode","request":request,
                "request_bytes":bytes,"usage_unknown":true,"estimated_usd":Value::Null
            })).map_err(|_| ())?;
                    budget.attempts += 1;
                    budget.unknown_attempts += 1;
                    budget.request_bytes += bytes;
                    Ok(())
                })
                .await;
        let disposition = match selection {
            Ok(selection) => {
                record_attempt(
                    &mut trace,
                    &mut budget,
                    1,
                    client::Route::OpenCode,
                    &selection.attempt,
                    selection.usage.as_ref(),
                    "success",
                    selection.request.clone(),
                    Some(selection.response.clone()),
                )
                .unwrap();
                let fresh: Vec<_> = paths
                    .iter()
                    .map(|path| std::fs::read_to_string(root.join(path)).unwrap())
                    .collect();
                let sources: Vec<_> = paths
                    .iter()
                    .zip(&fresh)
                    .map(|(p, t)| (p.as_path(), t.as_str()))
                    .collect();
                let effect = match packet.select(&selection, &frame(), &sources) {
                    Ok(Some(Effect::Bundle(bundle))) => {
                        serde_json::json!({"kind":"bundle","bundle":bundle})
                    }
                    Ok(Some(Effect::Read(index))) => {
                        serde_json::json!({"kind":"read_candidate","index":index})
                    }
                    Ok(None) => serde_json::json!({"kind":"inactive"}),
                    Err(reason) => serde_json::json!({"kind":"refused","reason":reason}),
                };
                serde_json::json!({"selected_id":selection.selected_id,"confidence":selection.confidence,"effect":effect})
            }
            Err(failure) => {
                let usage = if failure.usage_unknown {
                    None
                } else {
                    failure.response.as_ref().and_then(|r| r.get("usage"))
                };
                record_attempt(
                    &mut trace,
                    &mut budget,
                    1,
                    client::Route::OpenCode,
                    &failure.attempt,
                    usage,
                    failure.category,
                    failure.request.clone().unwrap_or(Value::Null),
                    failure.response.clone(),
                )
                .unwrap();
                serde_json::json!({"kind":"transport_failure","reason":failure.category})
            }
        };
        trace
            .record(serde_json::json!({"event":"utility_probe_disposition","result":disposition}))
            .unwrap();
        trace.record(serde_json::json!({
            "event":"episode_summary","decisions":budget.calls,"physical_send_intents":budget.attempts,
            "unknown_attempts":budget.unknown_attempts,"tokens":token_totals(&budget),
            "request_bytes":budget.request_bytes,"response_bytes":budget.response_bytes,
            "latencies_ms":budget.latencies_ms,"known_estimated_usd":budget.estimated_usd,
            "actual_provider_charge":Value::Null
        })).unwrap();
    }
}
