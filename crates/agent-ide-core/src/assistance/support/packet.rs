//! Bounded packet serialization, private prepared-artifact loading and the offline prototype.
//! Artifact loading grants no source access; callers prove source authority and delivery.

use serde_json::Value;

/// Serializes actual JSON bytes under the existing 8 KiB state ceiling.
pub(super) fn encode(state: &Value) -> Result<String, &'static str> {
    let encoded = serde_json::to_string(state).map_err(|_| "state_encoding")?;
    if encoded.len() > 8 * 1024 {
        return Err("state_budget");
    }
    Ok(encoded)
}

/// Trusted operator output from a separately measured Jev context selection.
/// It grants no source access: activation re-observes every file and verifies every window.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Prepared {
    /// Fixed private artifact format, independent from the model's response protocol.
    schema: String,
    /// Exact main-task text that produced the context.
    task: String,
    /// The existing configured route's exact model identifier.
    model: String,
    /// Only a selected code-bearing bundle is deliverable.
    selected_id: String,
    /// Validated Choice confidence, not an estimate of coding quality.
    confidence: f64,
    /// Complete byte identities plus the exact partial windows to disclose.
    pub(super) evidence: Vec<PreparedWindow>,
    /// Observed public run facts, not evidence that this actor performed a test.
    run_facts: Value,
    /// Explicit unresolved coverage and contract questions.
    gaps: Vec<String>,
}

/// One prepared excerpt; all fields are checked against an authorized source read.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreparedWindow {
    /// Worktree-relative path, never a provider-generated command or absolute path.
    pub(super) path: std::path::PathBuf,
    /// BLAKE3 of the complete file, rather than only this excerpt.
    version: String,
    /// Exact UTF-8 window bounds, in bytes.
    start_byte: usize,
    /// Exclusive UTF-8 end of the partial window.
    end_byte: usize,
    /// Literal project text; it cannot change an execution recipe.
    text: String,
    /// This slice makes no stronger semantic or test-coverage claim.
    relation: String,
    /// Actual complete file length expected at activation.
    source_bytes: usize,
}

impl Prepared {
    /// Reads one private no-follow operator artifact; never constructs a client or opens a key.
    pub(super) fn load(
        path: &std::path::Path,
        trace_dir: &std::path::Path,
        task: &str,
        model: &str,
    ) -> Result<Self, &'static str> {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        if !path.is_absolute()
            || path.canonicalize().ok().as_deref() != Some(path)
            || trace_dir.canonicalize().ok().as_deref() != Some(trace_dir)
            || !path.starts_with(trace_dir)
        {
            return Err("prepared_path");
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| "prepared_file")?;
        let metadata = file.metadata().map_err(|_| "prepared_file")?;
        // SAFETY: geteuid only returns this process's effective user identity.
        let uid = unsafe { libc::geteuid() };
        let directory = std::fs::metadata(trace_dir).map_err(|_| "prepared_path")?;
        if !directory.is_dir() || directory.uid() != uid || directory.mode() & 0o077 != 0 {
            return Err("prepared_path");
        }
        if !metadata.is_file()
            || metadata.uid() != uid
            || metadata.mode() & 0o077 != 0
            || metadata.len() == 0
            || metadata.len() > 24 * 1024
        {
            return Err("prepared_file");
        }
        let mut bytes = Vec::new();
        file.take(24 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "prepared_file")?;
        if bytes.len() > 24 * 1024 {
            return Err("prepared_budget");
        }
        let prepared: Self = serde_json::from_slice(&bytes).map_err(|_| "prepared_format")?;
        if prepared.schema != "jev-prepared-context-v1"
            || prepared.task != task
            || prepared.model != model
            || prepared.selected_id != "bundle"
            || !(0.5..=1.0).contains(&prepared.confidence)
        {
            return Err("prepared_identity");
        }
        if prepared.evidence.is_empty() || prepared.evidence.len() > 4 {
            return Err("prepared_budget");
        }
        for window in &prepared.evidence {
            if !crate::workspace::observation::valid_relative_path(&window.path)
                || window.source_bytes > 64 * 1024
                || window.start_byte > window.end_byte
                || window.end_byte > window.source_bytes
                || window.text.len() > 2048
                || window.relation != "observed_only; not proof of test coverage"
            {
                return Err("prepared_window");
            }
        }
        prepared.render()?;
        Ok(prepared)
    }

    /// Refuses stale bytes and partial UTF-8 before any prepared code can be retained or sent.
    pub(super) fn matches(&self, path: &std::path::Path, bytes: &[u8]) -> bool {
        let digest = blake3::hash(bytes).to_hex().to_string();
        let windows: Vec<_> = self
            .evidence
            .iter()
            .filter(|window| window.path == path)
            .collect();
        !windows.is_empty()
            && windows.into_iter().all(|window| {
                bytes.len() == window.source_bytes
                    && digest == window.version
                    && std::str::from_utf8(bytes)
                        .ok()
                        .and_then(|source| source.get(window.start_byte..window.end_byte))
                        == Some(window.text.as_str())
            })
    }

    /// Renders evidence only; original fixture authority and provider claims are never replayed.
    pub(super) fn render(&self) -> Result<String, &'static str> {
        let value = serde_json::json!({
            "source_windows":self.evidence,
            "observed_public_run_facts":self.run_facts,
            "gaps":self.gaps,
            "coverage":"partial source evidence; not a task-completion verdict",
        });
        let encoded = encode(&value)?;
        Ok(format!(
            "Jev context evidence (partial; intended contract unresolved):\n{encoded}"
        ))
    }
}

/// Off-model packet-construction prototype and operator measurement.
/// Runtime consumes only the separately validated Prepared artifact through real authority.
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
    /// Reads one current tracked public source without symlinks, bounded to receiver capacity.
    fn public_source(root: &Path, path: &Path, tracked: &[u8]) -> Result<String, &'static str> {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        if !crate::workspace::observation::valid_relative_path(path)
            || !tracked
                .split(|byte| *byte == 0)
                .any(|item| item == path.as_os_str().as_encoded_bytes())
            || root.join(path).canonicalize().ok() != Some(root.join(path))
        {
            return Err("source_not_tracked_canonical");
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(root.join(path))
            .map_err(|_| "source_open")?;
        if !file.metadata().map_err(|_| "source_metadata")?.is_file() {
            return Err("source_type");
        }
        let mut bytes = Vec::new();
        file.take(65537)
            .read_to_end(&mut bytes)
            .map_err(|_| "source_read")?;
        if bytes.len() > 65536 {
            return Err("source_budget");
        }
        String::from_utf8(bytes).map_err(|_| "source_utf8")
    }

    /// Selects a small real failure-line window; a filename counterpart is only a lexical relation.
    fn public_window(path: &Path, text: &str, line: usize) -> Result<Window, &'static str> {
        let start = text
            .split_inclusive('\n')
            .take(line.saturating_sub(9))
            .map(str::len)
            .sum::<usize>();
        let mut end = (start + 1536).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Window::capture(path, text, start..end)
    }

    /// Runs a generic two-choice operator episode from actual public failures; never edits code.
    #[tokio::test]
    #[ignore = "operator-only configured context preparation; explicit offline or live invocation"]
    async fn manual_configured_context_preparation() {
        use super::super::{Budget, Trace, parse_config, record_attempt, token_totals};
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let read_json = |name: &str| -> Value {
            let path = std::env::var(name).unwrap();
            let bytes = std::fs::read(path).unwrap();
            assert!(bytes.len() <= 24 * 1024);
            serde_json::from_slice(&bytes).unwrap()
        };
        let input = read_json("AGENT_IDE_JEV_CONTEXT_INPUT");
        let mut settings = read_json("AGENT_IDE_JEV_CONTEXT_CONFIG");
        let config = parse_config(Some(&settings)).unwrap().unwrap();
        assert_eq!(input["schema"], "jev-cold-public-input-v1");
        assert_eq!(input["task"], config.task);
        let output = PathBuf::from(std::env::var("AGENT_IDE_JEV_CONTEXT_OUT").unwrap());
        assert_eq!(output.canonicalize().unwrap(), output);
        assert!(output.starts_with("/Users/pluto/projects/tmp/agent-ide/jev-context"));
        let root = PathBuf::from(input["worktree"].as_str().unwrap());
        assert_eq!(root.canonicalize().unwrap(), root);
        let git = |args: &[&str]| {
            let reply = std::process::Command::new("/usr/bin/git")
                .args(["-C"])
                .arg(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(reply.status.success());
            reply.stdout
        };
        assert_eq!(
            String::from_utf8(git(&["rev-parse", "HEAD"]))
                .unwrap()
                .trim(),
            input["root_commit"].as_str().unwrap()
        );
        assert!(git(&["status", "--porcelain"]).is_empty());
        let tracked = git(&["ls-files", "-z"]);
        let candidates = input["candidates"].as_array().unwrap();
        assert!(!candidates.is_empty() && candidates.len() <= 10);
        let mut options = vec![client::Candidate {
            id: "none".into(),
            description: "No useful supported context action".into(),
            auto_test: false,
        }];
        for candidate in candidates {
            let path = Path::new(candidate["path"].as_str().unwrap());
            assert!(crate::workspace::observation::valid_relative_path(path));
            options.push(client::Candidate { id:candidate["id"].as_str().unwrap().into(),
                description:format!("Read observed public-failure candidate {path:?}; fault and intended contract unproven"), auto_test:false });
        }
        let route = config.selected_route();
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(output.join("episode.jsonl"))
            .unwrap();
        let mut trace = Trace(file);
        let live = std::env::var("AGENT_IDE_JEV_CONTEXT_LIVE").unwrap() == "1";
        trace
            .record(
                serde_json::json!({"event":"operator_context_inputs","live":live,"input":input,
            "endpoint":route.endpoint(),"model":route.model(),"main_delivery":"not_recorded"}),
            )
            .unwrap();
        let client = live.then(|| {
            client::JevClient::from_credential_file(&config.credential_file, route).unwrap()
        });
        let began = std::time::Instant::now();
        let mut budget = Budget::default();
        let mut state = input.clone();
        state.as_object_mut().unwrap().remove("worktree");
        state.as_object_mut().unwrap().remove("root_commit");
        let mut windows: Vec<Window> = Vec::new();
        let mut published = false;
        for decision in 1..=2 {
            assert!(began.elapsed() < std::time::Duration::from_secs(5));
            for window in &windows {
                let text = public_source(&root, &window.path, &tracked).unwrap();
                assert_eq!(
                    blake3::hash(text.as_bytes()).to_hex().as_str(),
                    window.version
                );
            }
            let encoded = encode(&state).unwrap();
            let selection = if let Some(client) = &client {
                budget.calls += 1;
                let result = client.decide(&encoded,&options,|request| {
                    trace.record(serde_json::json!({"event":"before_send","decision":decision,"request":request,"usage_unknown":true})).map_err(|_| ())?;
                    budget.attempts += 1; budget.unknown_attempts += 1;
                    budget.request_bytes += serde_json::to_vec(request).map_err(|_| ())?.len();
                    Ok(())
                }).await;
                match result {
                    Ok(value) => {
                        record_attempt(
                            &mut trace,
                            &mut budget,
                            decision,
                            route,
                            &value.attempt,
                            value.usage.as_ref(),
                            "success",
                            value.request.clone(),
                            Some(value.response.clone()),
                        )
                        .unwrap();
                        value
                    }
                    Err(failure) => {
                        let usage = if failure.usage_unknown {
                            None
                        } else {
                            failure.response.as_ref().and_then(|v| v.get("usage"))
                        };
                        record_attempt(
                            &mut trace,
                            &mut budget,
                            decision,
                            route,
                            &failure.attempt,
                            usage,
                            failure.category,
                            failure.request.clone().unwrap_or(Value::Null),
                            failure.response.clone(),
                        )
                        .unwrap();
                        break;
                    }
                }
            } else {
                let id = if decision == 1 {
                    candidates[0]["id"].as_str().unwrap()
                } else {
                    "bundle"
                };
                trace.record(serde_json::json!({"event":"offline_scripted_choice","decision":decision,"id":id})).unwrap();
                selected(id, 0.75)
            };
            if began.elapsed() >= std::time::Duration::from_secs(5) || selection.confidence < 0.5 {
                break;
            }
            if selection.selected_id == "none" {
                break;
            }
            if decision == 1 {
                let item = candidates
                    .iter()
                    .find(|v| v["id"].as_str() == Some(selection.selected_id.as_str()))
                    .unwrap();
                let path = PathBuf::from(item["path"].as_str().unwrap());
                let mut paths = vec![(path.clone(), item["line"].as_u64().unwrap() as usize)];
                let spelling = path.to_str().unwrap();
                let counterpart = if spelling.ends_with(".test.ts") {
                    spelling.replace(".test.ts", ".ts")
                } else {
                    spelling.trim_end_matches(".ts").to_owned() + ".test.ts"
                };
                if tracked
                    .split(|b| *b == 0)
                    .any(|p| p == counterpart.as_bytes())
                {
                    let line = candidates
                        .iter()
                        .find(|v| v["path"].as_str() == Some(counterpart.as_str()))
                        .and_then(|v| v["line"].as_u64())
                        .unwrap_or(1);
                    paths.push((PathBuf::from(counterpart), line as usize));
                }
                for (path, line) in paths {
                    let text = public_source(&root, &path, &tracked).unwrap();
                    windows.push(public_window(&path, &text, line).unwrap());
                }
                let mut facts = input["run_facts"].clone();
                if let Some(failures) = facts["failures"].as_array_mut() {
                    for failure in failures {
                        if let Some(text) = failure["failure"].as_str() {
                            failure["failure"] =
                                serde_json::json!(text.chars().take(350).collect::<String>());
                        }
                    }
                }
                state = serde_json::json!({"phase":"context","task":config.task,"evidence":windows,"run_facts":facts,
                    "gaps":["Public tests may be incomplete or incorrect; intended public contracts remain unresolved.","Filename counterpart is lexical evidence, not proven test coverage."],"main_delivery":"not_recorded"});
                options = vec![client::Candidate{id:"none".into(),description:"No useful evidence to give the coding agent".into(),auto_test:false},
                    client::Candidate{id:"bundle".into(),description:"Give the coding agent these concrete source windows and public failure facts, with explicit unresolved contracts".into(),auto_test:false}];
            } else if selection.selected_id == "bundle" {
                for window in &windows {
                    let text = public_source(&root, &window.path, &tracked).unwrap();
                    assert_eq!(
                        blake3::hash(text.as_bytes()).to_hex().as_str(),
                        window.version
                    );
                }
                let prepared = serde_json::json!({"schema":"jev-prepared-context-v1","task":config.task,"model":route.model(),
                    "selected_id":"bundle","confidence":selection.confidence,"evidence":windows,
                    "run_facts":state["run_facts"],"gaps":state["gaps"]});
                let artifact = output.join("prepared.json");
                let mut file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&artifact)
                    .unwrap();
                file.write_all(&serde_json::to_vec(&prepared).unwrap())
                    .unwrap();
                super::Prepared::load(&artifact, &output, &config.task, route.model()).unwrap();
                settings["trace_dir"] = serde_json::json!(output);
                settings["prepared_context_file"] = serde_json::json!(artifact);
                let mut file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(output.join("launcher-section.json"))
                    .unwrap();
                file.write_all(&serde_json::to_vec_pretty(&settings).unwrap())
                    .unwrap();
                published = true;
            }
        }
        trace.record(serde_json::json!({"event":"operator_context_summary","live":live,"prepared":published,"decisions":budget.calls,
            "physical_attempts":budget.attempts,"unknown_attempts":budget.unknown_attempts,"tokens":token_totals(&budget),
            "latencies_ms":budget.latencies_ms,"estimated_usd":budget.estimated_usd,"seconds":began.elapsed().as_secs_f64()})).unwrap();
        assert!(
            published,
            "No prepared context; preserved disposition is not a successful Jev-assisted trial"
        );
    }
}
