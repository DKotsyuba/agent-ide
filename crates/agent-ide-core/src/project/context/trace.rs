//! Project-private, append-only preparation evidence. Records facts, never credentials or vectors.
//!
//! Tracing fails open: an IO failure degrades the trace (visible through
//! [`PrepareTrace::degradation`]) and stops further writes, but never fails the preparation it
//! observes. A degraded trace never claims to be complete.

use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_TRACE: AtomicU64 = AtomicU64::new(0);

/// What the worker states about delivery when it finishes: its own reply only.
const HOST_VISIBILITY: &str = "unverified; worker output is not external client receipt";

/// One invocation, shared through retrieval and final worker assembly, with ordered events.
#[derive(Clone, Debug)]
pub(crate) struct PrepareTrace(Arc<Mutex<State>>);

/// The invocation's journal, or why it stopped being written.
#[derive(Debug)]
struct State {
    /// The open journal while writes succeed; `None` once degraded.
    sink: Option<Sink>,
    /// First failure as `operation: ErrorKind`; never a path, OS message or payload.
    degradation: Option<String>,
}

/// Owned private file and its write position.
#[derive(Debug)]
struct Sink {
    file: File,
    path: PathBuf,
    sequence: u64,
    /// Bytes successfully written so far.
    bytes: u64,
    finished: bool,
}

impl PrepareTrace {
    /// Creates a unique journal below the index's already verified project state directory.
    /// Never fails: a directory or file that cannot be created privately yields a degraded
    /// trace whose [`Self::degradation`] says so, and that writes nothing.
    pub(crate) fn new(root: &Path) -> io::Result<Self> {
        let state = match open(root) {
            Ok(sink) => State {
                sink: Some(sink),
                degradation: None,
            },
            Err((operation, error)) => State {
                sink: None,
                degradation: Some(describe(operation, &error)),
            },
        };
        Ok(Self(Arc::new(Mutex::new(state))))
    }

    /// Writes one complete JSON line. A write failure degrades the trace and is not returned:
    /// audit IO never fails the preparation. After degradation this is a no-op.
    pub(crate) fn record(&self, stage: &str, data: Value) -> io::Result<()> {
        self.lock().write(stage, data);
        Ok(())
    }

    /// Marks the worker's completed reply, not receipt or visibility at the external model host.
    /// Adds the bytes written before this event and, when absent, the host-visibility caveat,
    /// then syncs the file. A failure degrades the trace, which then never counts as finished.
    pub(crate) fn finish(&self, data: Value) -> io::Result<()> {
        let mut state = self.lock();
        let Some(bytes) = state.sink.as_ref().map(|sink| sink.bytes) else {
            return Ok(());
        };
        let data = match data {
            Value::Object(mut fields) => {
                fields.insert("trace_bytes_before_finish".into(), json!(bytes));
                fields
                    .entry("host_visibility")
                    .or_insert_with(|| json!(HOST_VISIBILITY));
                Value::Object(fields)
            }
            other => json!({"result": other, "trace_bytes_before_finish": bytes,
                "host_visibility": HOST_VISIBILITY}),
        };
        if state.write("worker_reply_ready", data) {
            match state.sink.as_mut().map(|sink| sink.file.sync_all()) {
                Some(Ok(())) => {
                    if let Some(sink) = state.sink.as_mut() {
                        sink.finished = true;
                    }
                }
                Some(Err(error)) => state.degrade("sync", &error),
                None => {}
            }
        }
        Ok(())
    }

    /// Why this trace is incomplete, as `operation: ErrorKind`; `None` while it is intact.
    pub(crate) fn degradation(&self) -> Option<String> {
        self.lock().degradation.clone()
    }

    /// The state. A panic in another holder may have interrupted an append, so a poisoned lock
    /// degrades the trace (`lock: poisoned`) and stops it; the preparation itself continues.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|poisoned| {
            let mut state = poisoned.into_inner();
            state.close("lock: poisoned".to_owned());
            state
        })
    }
}

impl State {
    /// Appends one event; on failure records the degradation, closes the journal and returns
    /// false. Returns false without writing once degraded.
    fn write(&mut self, stage: &str, data: Value) -> bool {
        let Some(sink) = self.sink.as_mut() else {
            return false;
        };
        match sink.write(stage, data) {
            Ok(()) => true,
            Err((operation, error)) => {
                self.degrade(operation, &error);
                false
            }
        }
    }

    /// Stops writing after the first failure; the partial file keeps what was written.
    fn degrade(&mut self, operation: &str, error: &io::Error) {
        self.close(describe(operation, error));
    }

    /// Records the first degradation reason and closes the journal without another line: a
    /// degraded journal must not later gain a line that looks like a normal ending.
    fn close(&mut self, reason: String) {
        if self.degradation.is_none() {
            self.degradation = Some(reason);
        }
        if let Some(mut sink) = self.sink.take() {
            sink.finished = true;
        }
    }
}

impl Sink {
    /// Serializes an event with a monotonically increasing per-invocation sequence.
    fn write(&mut self, stage: &str, data: Value) -> Result<(), (&'static str, io::Error)> {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| ("clock", io::Error::other(error)))?
            .as_millis();
        let event = json!({"schema":"agent-ide.prepare-trace.v1", "trace_id":self.path.file_name().and_then(|name| name.to_str()),
            "sequence": self.sequence, "timestamp_ms":timestamp_ms, "stage":stage, "data":data});
        let mut bytes =
            serde_json::to_vec(&event).map_err(|error| ("serialize", io::Error::other(error)))?;
        bytes.push(b'\n');
        self.file
            .write_all(&bytes)
            .and_then(|()| self.file.flush())
            .map_err(|error| ("write", error))?;
        self.sequence += 1;
        self.bytes += bytes.len() as u64;
        Ok(())
    }
}

impl Drop for Sink {
    /// Preserves an honest incomplete marker on errors, cancellation or abandoned results.
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.write(
                "incomplete",
                json!({"reason":"invocation ended without worker reply completion"}),
            );
        }
    }
}

/// Creates the private directory and a fresh journal, naming the operation that failed.
fn open(root: &Path) -> Result<Sink, (&'static str, io::Error)> {
    let directory = root.join(".agent-ide/prepare-traces");
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(("create directory", error)),
    }
    let metadata =
        fs::symlink_metadata(&directory).map_err(|error| ("inspect directory", error))?;
    // SAFETY: geteuid only reads this process's effective user identity.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err((
            "verify directory",
            io::Error::new(io::ErrorKind::PermissionDenied, "not a private directory"),
        ));
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| ("clock", io::Error::other(error)))?
        .as_nanos();
    let nonce = NEXT_TRACE.fetch_add(1, Ordering::Relaxed);
    let path = directory.join(format!("{stamp}-{}-{nonce}.jsonl", std::process::id()));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|error| ("create file", error))?;
    Ok(Sink {
        file,
        path,
        sequence: 0,
        bytes: 0,
        finished: false,
    })
}

/// `operation: ErrorKind` only: no path, OS message or payload reaches a reply.
fn describe(operation: &str, error: &io::Error) -> String {
    format!("{operation}: {:?}", error.kind())
}

#[cfg(test)]
mod tests {
    use super::*;

    impl PrepareTrace {
        /// A trace writing to `file`, for injecting a write fault through a read-only handle.
        fn with_file(file: File, path: PathBuf) -> Self {
            Self(Arc::new(Mutex::new(State {
                sink: Some(Sink {
                    file,
                    path,
                    sequence: 0,
                    bytes: 0,
                    finished: false,
                }),
                degradation: None,
            })))
        }
    }

    /// Parses every journal line of `path`.
    fn rows(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Each invocation is isolated; source-shaped data survives escaping without inventing
    /// completion, and the finish event carries its byte count and the host caveat.
    #[test]
    fn traces_are_private_ordered_and_distinguish_incomplete() {
        let root = crate::scratch::ScratchDir::new("prepare-trace");
        fs::create_dir(root.join(".agent-ide")).unwrap();
        let first = PrepareTrace::new(&root).unwrap();
        first
            .record("selected", json!({"path":"a\"\n.rs","hash":"abc"}))
            .unwrap();
        let second = PrepareTrace::new(&root).unwrap();
        second.record("started", json!({})).unwrap();
        first.finish(json!({"operation_ref":"r"})).unwrap();
        assert_eq!(first.degradation(), None);
        drop(first);
        drop(second);
        let mut terminal = Vec::new();
        for entry in fs::read_dir(root.join(".agent-ide/prepare-traces")).unwrap() {
            let path = entry.unwrap().path();
            assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
            let rows = rows(&path);
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0]["sequence"], 0);
            assert_eq!(rows[1]["sequence"], 1);
            if rows[1]["stage"] == "worker_reply_ready" {
                let text = fs::read_to_string(&path).unwrap();
                let first_line = text.lines().next().unwrap().len() as u64 + 1;
                assert_eq!(rows[1]["data"]["trace_bytes_before_finish"], first_line);
                assert_eq!(rows[1]["data"]["host_visibility"], HOST_VISIBILITY);
                assert_eq!(rows[1]["data"]["operation_ref"], "r");
            }
            terminal.push(rows[1]["stage"].as_str().unwrap().to_owned());
        }
        terminal.sort();
        assert_eq!(terminal, ["incomplete", "worker_reply_ready"]);
    }

    /// A journal that cannot be created degrades without failing: every call still succeeds,
    /// nothing is written, and the reason names only the operation and the error kind.
    #[test]
    fn an_unusable_trace_directory_degrades_without_failing() {
        let root = crate::scratch::ScratchDir::new("prepare-trace-blocked");
        fs::create_dir(root.join(".agent-ide")).unwrap();
        // A regular file where the private directory belongs.
        fs::write(root.join(".agent-ide/prepare-traces"), "not a directory").unwrap();
        let trace = PrepareTrace::new(&root).unwrap();
        assert_eq!(
            trace.degradation().as_deref(),
            Some("verify directory: PermissionDenied")
        );
        trace.record("started", json!({"task":"x"})).unwrap();
        trace.finish(json!({})).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(".agent-ide/prepare-traces")).unwrap(),
            "not a directory"
        );
        let missing = crate::scratch::ScratchDir::new("prepare-trace-no-state");
        let trace = PrepareTrace::new(&missing).unwrap();
        assert_eq!(
            trace.degradation().as_deref(),
            Some("create directory: NotFound")
        );
    }

    /// A write fault mid-invocation keeps what was written, stops further writes, never claims
    /// completion or appends an incomplete line, and exposes only the operation and error kind.
    #[test]
    fn a_write_fault_degrades_the_trace_and_stops_writing() {
        let root = crate::scratch::ScratchDir::new("prepare-trace-fault");
        let path = root.join("trace.jsonl");
        fs::write(&path, "").unwrap();
        // A read-only handle: every write fails with the operating system's own error.
        let file = OpenOptions::new().read(true).open(&path).unwrap();
        let trace = PrepareTrace::with_file(file, path.clone());
        trace.record("started", json!({})).unwrap();
        let reason = trace
            .degradation()
            .expect("a write fault degrades the trace");
        assert!(reason.starts_with("write: "), "{reason}");
        assert!(!reason.contains(path.to_str().unwrap()));
        trace.record("later", json!({})).unwrap();
        trace.finish(json!({})).unwrap();
        assert_eq!(trace.degradation().as_deref(), Some(reason.as_str()));
        drop(trace);
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
    }

    /// A panic while another holder had the lock may have cut an append short: the trace then
    /// degrades and stops, so later records and finish cannot make it look complete.
    #[test]
    fn a_poisoned_lock_degrades_the_trace() {
        let root = crate::scratch::ScratchDir::new("prepare-trace-poison");
        fs::create_dir(root.join(".agent-ide")).unwrap();
        let trace = PrepareTrace::new(&root).unwrap();
        trace.record("started", json!({})).unwrap();
        let shared = Arc::clone(&trace.0);
        let panicked = std::thread::spawn(move || {
            let _held = shared.lock().unwrap();
            panic!("holder interrupted");
        })
        .join();
        assert!(panicked.is_err());
        trace.record("later", json!({})).unwrap();
        trace.finish(json!({})).unwrap();
        assert_eq!(trace.degradation().as_deref(), Some("lock: poisoned"));
        drop(trace);
        let path = fs::read_dir(root.join(".agent-ide/prepare-traces"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let stages: Vec<_> = rows(&path)
            .iter()
            .map(|row| row["stage"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(stages, ["started"]);
    }
}
