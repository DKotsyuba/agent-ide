//! The daily fault report: the plan's section (b) failure taxonomy over the error journals, as the
//! official report and alert (`cargo xtask fault-report`), replacing the `errstats.py` counter.
//!
//! Unit: one *terminal dispatch line* per tool call that reached a daemon (the `log_tool_reply`
//! line), plus one *front line* per call whose front outcome produced no typed reply and whose
//! request id has no daemon dispatch line. `pending` replies are intermediate and excluded from
//! the denominator; worker job-failure lines, host-binding guard lines, edit-gate lines and
//! eviction notes duplicate a call or are not calls. Every failed call is put in exactly one class
//! of four kinds: `fault` (a known mechanism), `unexplained` (the journal cannot attribute it;
//! counted with `fault` in the conservative IDE-fault numerator), `caller` (the request was
//! wrong) and `honest` (a correct refusal of real state).
//!
//! Journals written before the dispatch-context fields (no `version`) are read with the original
//! heuristic, so the frozen section (b) input reproduces its published numbers exactly; newer
//! lines are recognised by their `version` field. Only closed fields are read; no raw record is
//! ever printed.
//!
//! Input: `<root>/<key>/events.jsonl` (with its rotated `events.jsonl.1`) or flat `<root>/<key>.jsonl`
//! copies. Std and `serde_json` only.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::Result;

/// The eleven tool methods a dispatch line can carry.
const TOOLS: [&str; 11] = [
    "start", "context", "diff", "edit", "inspect", "stop", "outline", "read", "symbol", "graph",
    "test",
];
/// Default minimum number of terminal calls before the rate alert can fire.
const DEFAULT_MIN_CALLS: usize = 300;
/// Default share of unexplained faults (percent) above which the instrumentation backlog warns.
const DEFAULT_UNEXPLAINED_PERCENT: f64 = 0.1;
/// Default window of a report without `--since`, in days.
const DEFAULT_DAYS: u64 = 7;
/// Classes with fewer events than this never raise the day-over-day doubling warning.
const DOUBLING_MIN_EVENTS: usize = 10;

/// The four kinds of a failed call, plus the success and degraded-success columns.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Kind {
    /// The call completed through the full path.
    Ok,
    /// The call completed through a weaker path (lexical answer, unknown diagnostics).
    Degraded,
    /// A known failure mechanism.
    Fault,
    /// An IDE failure the journal cannot attribute.
    Unexplained,
    /// The request itself was wrong.
    Caller,
    /// A correct refusal that reports real state.
    Honest,
    /// No rule matched.
    Unclassified,
}

impl Kind {
    /// The lowercase tag printed in the class table.
    fn tag(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Fault => "fault",
            Self::Unexplained => "unexplained",
            Self::Caller => "caller",
            Self::Honest => "honest",
            Self::Unclassified => "unclassified",
        }
    }
}

/// Which journal keys a report counts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scope {
    /// Every key.
    All,
    /// Keys that name a worktree outside the qualification locations (real work). The default.
    Field,
    /// Keys whose every worktree is a qualification location (gate, matrix, acceptance clones).
    Test,
}

/// Parsed command line of `fault-report`.
#[derive(Debug)]
pub struct Options {
    /// Journal directory; `AGENT_IDE_LOG_ROOT` or `~/.agent-ide/logs` when absent.
    root: Option<PathBuf>,
    /// First day counted (`YYYY-MM-DD`); the window start otherwise derived from `days`.
    since: Option<String>,
    /// Last day (`YYYY-MM-DD`, inclusive) or RFC 3339 instant (inclusive); now when absent.
    until: Option<String>,
    /// Window length in days when `since` is absent.
    days: u64,
    /// Which keys are counted.
    scope: Scope,
    /// Alert (fail the command) when the IDE-fault share (percent) exceeds this with enough calls.
    alert_threshold: Option<f64>,
    /// Calls the window needs before the rate alert can fire.
    min_calls: usize,
    /// Warn when the unexplained share (percent) exceeds this.
    unexplained_threshold: f64,
}

/// The usage line of `fault-report`.
pub const USAGE: &str = "cargo xtask fault-report [--root DIR] [--since YYYY-MM-DD] [--until DAY|RFC3339] \
[--days N] [--scope field|test|all] [--alert-threshold PERCENT] [--min-calls N] [--unexplained-threshold PERCENT]";

/// Parses the arguments after `fault-report`.
pub fn parse(args: &[String]) -> Result<Options> {
    let mut options = Options {
        root: None,
        since: None,
        until: None,
        days: DEFAULT_DAYS,
        scope: Scope::Field,
        alert_threshold: None,
        min_calls: DEFAULT_MIN_CALLS,
        unexplained_threshold: DEFAULT_UNEXPLAINED_PERCENT,
    };
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let mut value = || -> Result<&String> {
            args.next()
                .ok_or_else(|| format!("{flag} needs a value; usage: {USAGE}").into())
        };
        match flag.as_str() {
            "--root" => options.root = Some(PathBuf::from(value()?)),
            "--since" => options.since = Some(checked_day(value()?)?),
            "--until" => options.until = Some(checked_until(value()?)?),
            "--days" => {
                options.days = value()?.parse()?;
                if options.days == 0 {
                    return Err(format!("--days must be at least 1; usage: {USAGE}").into());
                }
            }
            "--scope" => {
                options.scope = match value()?.as_str() {
                    "all" => Scope::All,
                    "field" => Scope::Field,
                    "test" => Scope::Test,
                    other => return Err(format!("unknown scope {other}; usage: {USAGE}").into()),
                }
            }
            "--alert-threshold" => options.alert_threshold = Some(percentage(value()?)?),
            "--min-calls" => options.min_calls = value()?.parse()?,
            "--unexplained-threshold" => options.unexplained_threshold = percentage(value()?)?,
            other => return Err(format!("unknown argument {other}; usage: {USAGE}").into()),
        }
    }
    Ok(options)
}

/// Runs the report, prints it, and fails (non-zero exit) when an alert fired.
pub fn run(options: &Options) -> Result<()> {
    let root = options.root.clone().unwrap_or_else(default_root);
    let now = now_rfc3339();
    let until = options.until.clone().unwrap_or_else(|| now.clone());
    let since = options
        .since
        .clone()
        .unwrap_or_else(|| day_before(&until, options.days));
    if since.get(..10).unwrap_or("") > until.get(..10).unwrap_or("") {
        return Err(
            format!("the window is empty: --since {since} is after --until {until}").into(),
        );
    }
    let report = Report::build(&root, &since, &until, options.scope)?;
    print!("{}", report.render(&root, &since, &until, options.scope));
    for warning in report.warnings(options) {
        println!("WARN: {warning}");
    }
    let alerts = report.alerts(options);
    for alert in &alerts {
        println!("ALERT: {alert}");
    }
    if alerts.is_empty() {
        Ok(())
    } else {
        Err(format!("{} alert(s) fired", alerts.len()).into())
    }
}

/// The journal root when `--root` is absent: `AGENT_IDE_LOG_ROOT`, else
/// `$AGENT_IDE_HOME/.agent-ide/logs` (the product's own absolute per-user override), else
/// `$HOME/.agent-ide/logs`. A host that substitutes `HOME` (an agent runner) must pass `--root`;
/// the chosen root is the first line of the report.
fn default_root() -> PathBuf {
    let home = std::env::var_os("AGENT_IDE_HOME")
        .filter(|home| Path::new(home).is_absolute())
        .or_else(|| std::env::var_os("HOME"))
        .unwrap_or_default();
    std::env::var_os("AGENT_IDE_LOG_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join(".agent-ide").join("logs"))
}

/// A real calendar day `YYYY-MM-DD` (a leap day only in a leap year), or an error.
fn checked_day(text: &str) -> Result<String> {
    let parse =
        |range: std::ops::Range<usize>| text.get(range).and_then(|part| part.parse::<i64>().ok());
    let shape = text.len() == 10
        && text.as_bytes()[4] == b'-'
        && text.as_bytes()[7] == b'-'
        && text
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit());
    let valid = shape
        && matches!(
            (parse(0..4), parse(5..7), parse(8..10)),
            (Some(year), Some(month @ 1..=12), Some(day @ 1..=31))
                if civil_from_days(days_from_civil(year, month as u32, day as u32))
                    == (year, month as u32, day as u32)
        );
    if valid {
        Ok(text.to_owned())
    } else {
        Err(format!("{text:?} is not a calendar day YYYY-MM-DD; usage: {USAGE}").into())
    }
}

/// A calendar day, or a UTC instant `YYYY-MM-DDTHH:MM:SSZ` on a real calendar day, or an error.
fn checked_until(text: &str) -> Result<String> {
    if text.len() == 10 {
        return checked_day(text);
    }
    let clock = text.get(10..).unwrap_or("");
    let digits = |range: std::ops::Range<usize>| {
        clock
            .get(range)
            .filter(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|part| part.parse::<u32>().ok())
    };
    let valid = text.len() == 20
        && clock.len() == 10
        && clock.as_bytes()[0] == b'T'
        && clock.as_bytes()[3] == b':'
        && clock.as_bytes()[6] == b':'
        && clock.ends_with('Z')
        && matches!(
            (digits(1..3), digits(4..6), digits(7..9)),
            (Some(0..=23), Some(0..=59), Some(0..=59))
        )
        && text.get(..10).is_some_and(|day| checked_day(day).is_ok());
    if valid {
        Ok(text.to_owned())
    } else {
        Err(
            format!("{text:?} is not a day or a UTC instant YYYY-MM-DDTHH:MM:SSZ; usage: {USAGE}")
                .into(),
        )
    }
}

/// A finite percentage in `0..=100`, or an error (a NaN would silently disable its alert).
fn percentage(text: &str) -> Result<f64> {
    let value: f64 = text.parse()?;
    if value.is_finite() && (0.0..=100.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{text:?} is not a percentage between 0 and 100; usage: {USAGE}").into())
    }
}

/// One counted terminal call with the facts the tables need.
struct Call {
    /// Day (`YYYY-MM-DD`).
    day: String,
    /// Tool method tag.
    method: String,
    /// Journal key (one repository).
    key: String,
    /// Timestamp, for the outage streak.
    ts: String,
    /// Class name (empty for a success).
    class: String,
    /// Kind of the call.
    kind: Kind,
}

/// Everything one scan of the journals yields.
pub struct Report {
    /// Terminal calls, in journal order per key.
    calls: Vec<Call>,
    /// Pending replies per day (excluded from the denominator).
    pending: BTreeMap<String, usize>,
    /// Pending settlement counters.
    settlement: Settlement,
    /// Daemon and hook lifecycle counters.
    lifecycle: Lifecycle,
    /// Number of journal keys counted.
    keys: usize,
    /// Journal lines that were not valid JSON and were skipped (disclosed in the report).
    malformed: usize,
    /// Internal probe lines (the front's actor query) kept out of the call counts.
    probes: usize,
}

/// What became of the calls that answered `pending`.
#[derive(Default)]
struct Settlement {
    /// Pending replies seen.
    pending: usize,
    /// Pending replies a later `ide.inspect` collected.
    collected: usize,
    /// Uncollected, and the job's completion record says it succeeded (completed or degraded).
    uncollected_completed: usize,
    /// Uncollected, and the job's completion record says it ended without success (a refused or
    /// unknown edit, a cancelled call).
    uncollected_refused: usize,
    /// Uncollected, and the job's failure line says it failed.
    uncollected_failed: usize,
    /// Uncollected with no completion or failure line (older journals, abandoned jobs).
    uncollected_unknown: usize,
}

/// Daemon and hook lifecycle counters of the window.
#[derive(Default)]
struct Lifecycle {
    /// Daemon `started` lines.
    daemon_starts: usize,
    /// Daemon `failed` and `fatal` lines by their closed failure detail.
    daemon_failed: BTreeMap<String, usize>,
    /// Client `reestablished` lines.
    client_reestablished: usize,
    /// Forced daemon replacements, counted as lifecycle events rather than tool calls.
    forced_replacements: usize,
    /// Panic evidence from daemon or job lines, counted once at the producer's journal site.
    panics: BTreeMap<String, usize>,
    /// Hook warn lines by `detail`.
    hook_warns: BTreeMap<String, usize>,
}

impl Report {
    /// Scans every journal under `root` once.
    fn build(root: &Path, since: &str, until: &str, scope: Scope) -> Result<Self> {
        let mut report = Self {
            calls: Vec::new(),
            pending: BTreeMap::new(),
            settlement: Settlement::default(),
            lifecycle: Lifecycle::default(),
            keys: 0,
            malformed: 0,
            probes: 0,
        };
        let journals = journal_files(root)?;
        if journals.is_empty() {
            return Err(format!("no journals found under {}", root.display()).into());
        }
        for (key, files) in journals {
            let mut records: Vec<Value> = Vec::new();
            for path in &files {
                // An unreadable journal is an error, not an empty one; invalid UTF-8 is replaced
                // (the reference classifier does the same) and a line that is not JSON is skipped
                // and counted, so a damaged input is disclosed instead of looking healthy.
                let bytes =
                    fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
                for line in String::from_utf8_lossy(&bytes)
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                {
                    match serde_json::from_str(line) {
                        Ok(record) => records.push(record),
                        Err(_) => report.malformed += 1,
                    }
                }
            }
            let test_key = is_test_key(&records);
            match scope {
                Scope::Field if test_key => continue,
                Scope::Test if !test_key => continue,
                _ => {}
            }
            report.keys += 1;
            report.scan_key(&key, &records, since, until);
        }
        Ok(report)
    }

    /// Counts one journal key's lines inside the window.
    fn scan_key(&mut self, key: &str, records: &[Value], since: &str, until: &str) {
        let first_call = self.calls.len();
        let in_window = |record: &Value| {
            let ts = text(record, "ts");
            let day = ts.get(..10).unwrap_or("");
            since <= day && (if until.len() > 10 { ts } else { day }) <= until
        };
        let window: Vec<&Value> = records.iter().filter(|record| in_window(record)).collect();
        let answered: HashSet<&str> = window
            .iter()
            .filter(|record| line_kind(record) == LineKind::Dispatch)
            .filter_map(|record| record.get("request")?.as_str())
            .collect();
        let mut collected: HashSet<&str> = HashSet::new();
        let mut completed: HashMap<&str, &Value> = HashMap::new();
        let mut failed: HashMap<&str, &Value> = HashMap::new();
        let mut pendings: Vec<&str> = Vec::new();
        for record in &window {
            let correlation = record.get("correlation").and_then(Value::as_str);
            match line_kind(record) {
                LineKind::Dispatch => {
                    // The product's own probe (the front's actor query), not an agent's call.
                    if record.get("probe").is_some() {
                        self.probes += 1;
                        continue;
                    }
                    let day = text(record, "ts").get(..10).unwrap_or("").to_owned();
                    let outcome = text(record, "outcome");
                    if outcome == "pending" {
                        *self.pending.entry(day).or_default() += 1;
                        self.settlement.pending += 1;
                        pendings.extend(correlation);
                        continue;
                    }
                    if delivers_result(record) {
                        collected.extend(correlation);
                    }
                    let (class, kind) = match outcome {
                        "completed" => (String::new(), Kind::Ok),
                        "degraded" => (String::new(), Kind::Degraded),
                        _ => classify_dispatch(record),
                    };
                    self.push(key, record, day, class, kind);
                }
                LineKind::Front => {
                    // The daemon answered this request too: its dispatch line is the call.
                    if record
                        .get("request")
                        .and_then(Value::as_str)
                        .is_some_and(|request| answered.contains(request))
                    {
                        continue;
                    }
                    let day = text(record, "ts").get(..10).unwrap_or("").to_owned();
                    let (class, kind) = classify_front(text(record, "detail"));
                    self.push(key, record, day, class, kind);
                }
                LineKind::Other => {
                    self.lifecycle_line(record);
                    if let Some(correlation) = correlation
                        && TOOLS.contains(&text(record, "method"))
                    {
                        if text(record, "detail") == "pending_completion" {
                            completed.insert(correlation, record);
                        } else if text(record, "outcome") != "completed" {
                            failed.insert(correlation, record);
                        }
                    }
                }
            }
        }
        for reference in pendings {
            if collected.contains(reference) {
                self.settlement.collected += 1;
                continue;
            }
            // A result nobody collected still ends the call: its own terminal row is counted
            // once. A completion record always identifies its call; a job-failure line does only
            // in the current format (it carries the call id), so older journals keep their
            // published, dispatch-only count.
            if let Some(record) = completed.get(reference) {
                let (class, kind) = match text(record, "outcome") {
                    "completed" => (String::new(), Kind::Ok),
                    "degraded" => (String::new(), Kind::Degraded),
                    _ => classify(text(record, "reason"), ""),
                };
                if matches!(kind, Kind::Ok | Kind::Degraded) {
                    self.settlement.uncollected_completed += 1;
                } else {
                    self.settlement.uncollected_refused += 1;
                }
                let day = text(record, "ts").get(..10).unwrap_or("").to_owned();
                self.push(key, record, day, class, kind);
            } else if let Some(record) = failed.get(reference) {
                self.settlement.uncollected_failed += 1;
                if record.get("request").is_some() {
                    let (class, kind) = classify(text(record, "reason"), text(record, "detail"));
                    let day = text(record, "ts").get(..10).unwrap_or("").to_owned();
                    self.push(key, record, day, class, kind);
                }
            } else {
                self.settlement.uncollected_unknown += 1;
            }
        }
        // Orphan terminal rows are appended after the lines that follow them in the journal; put
        // the key's calls in time order (stable) so the outage streak reads chronologically.
        self.calls[first_call..].sort_by(|a, b| a.ts.cmp(&b.ts));
    }

    /// Records one counted terminal call.
    fn push(&mut self, key: &str, record: &Value, day: String, class: String, kind: Kind) {
        self.calls.push(Call {
            day,
            method: text(record, "method").to_owned(),
            key: key.to_owned(),
            ts: text(record, "ts").to_owned(),
            class,
            kind,
        });
    }

    /// Counts daemon, client and hook lifecycle facts and panic evidence from any job, even
    /// when nobody collected its result. These events never add a terminal tool call.
    fn lifecycle_line(&mut self, record: &Value) {
        let detail = text(record, "detail");
        if detail.contains("panic at") {
            *self.lifecycle.panics.entry(detail.to_owned()).or_default() += 1;
        }
        match (text(record, "method"), text(record, "outcome")) {
            ("daemon", "started") => self.lifecycle.daemon_starts += 1,
            ("daemon", "failed" | "fatal") => {
                *self
                    .lifecycle
                    .daemon_failed
                    .entry(
                        if detail.is_empty() {
                            "(no detail)"
                        } else {
                            detail
                        }
                        .to_owned(),
                    )
                    .or_default() += 1;
            }
            ("client", "reestablished") => self.lifecycle.client_reestablished += 1,
            ("client", "failed") if detail.starts_with("wedged_daemon_replaced") => {
                self.lifecycle.forced_replacements += 1;
            }
            ("hook", _) if text(record, "level") == "warn" => {
                *self
                    .lifecycle
                    .hook_warns
                    .entry(
                        if detail.is_empty() {
                            "(no detail)"
                        } else {
                            detail
                        }
                        .to_owned(),
                    )
                    .or_default() += 1;
            }
            _ => {}
        }
    }

    /// Per-kind counts of the calls matching `keep`.
    fn tally(&self, keep: impl Fn(&Call) -> bool) -> BTreeMap<Kind, usize> {
        let mut counts = BTreeMap::new();
        for call in self.calls.iter().filter(|call| keep(call)) {
            *counts.entry(call.kind).or_default() += 1;
        }
        counts
    }

    /// The alerts this report raises under `options`: a rate above its threshold with enough
    /// calls, or a journaled panic. Any alert fails the command.
    fn alerts(&self, options: &Options) -> Vec<String> {
        let total = self.calls.len();
        let counts = self.tally(|_| true);
        let get = |kind: Kind| counts.get(&kind).copied().unwrap_or(0);
        let ide = get(Kind::Fault) + get(Kind::Unexplained);
        let mut alerts = Vec::new();
        if let Some(threshold) = options.alert_threshold
            && total >= options.min_calls
            && percent(ide, total) > threshold
        {
            alerts.push(format!(
                "IDE-fault rate {:.2}% exceeds {threshold:.2}% over {total} terminal calls",
                percent(ide, total)
            ));
        }
        for (detail, count) in &self.lifecycle.panics {
            alerts.push(format!("{count} panic(s) journaled: {detail}"));
        }
        alerts
    }

    /// The warnings of this report under `options`: they print but never fail the command.
    ///
    /// The unexplained share above its threshold is an instrumentation backlog (the calls stay in
    /// the IDE-fault numerator), and a fault class that doubled day over day with enough events
    /// is a trend to look at.
    fn warnings(&self, options: &Options) -> Vec<String> {
        let total = self.calls.len();
        let unexplained = self
            .tally(|_| true)
            .get(&Kind::Unexplained)
            .copied()
            .unwrap_or(0);
        let mut warnings = Vec::new();
        if total > 0 && percent(unexplained, total) > options.unexplained_threshold {
            warnings.push(format!(
                "unexplained faults {:.2}% exceed {:.2}% (instrumentation backlog; they stay in the numerator)",
                percent(unexplained, total),
                options.unexplained_threshold
            ));
        }
        if let Some((name, day, now, before)) = self.doubled_class() {
            warnings.push(format!(
                "class `{name}` doubled on {day}: {now} events after {before}"
            ));
        }
        warnings
    }

    /// The first fault class whose daily count at least doubled (with enough events) from the
    /// previous counted day, as `(class, day, count, previous count)`.
    fn doubled_class(&self) -> Option<(String, String, usize, usize)> {
        let days: Vec<String> = self
            .calls
            .iter()
            .map(|call| call.day.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut per_class: BTreeMap<(&str, &str), usize> = BTreeMap::new();
        for call in self
            .calls
            .iter()
            .filter(|call| matches!(call.kind, Kind::Fault | Kind::Unexplained))
        {
            *per_class.entry((&call.class, &call.day)).or_default() += 1;
        }
        let classes: BTreeSet<&str> = per_class.keys().map(|(class, _)| *class).collect();
        for class in classes {
            for pair in days.windows(2) {
                let count = |day: &str| per_class.get(&(class, day)).copied().unwrap_or(0);
                let (before, now) = (count(&pair[0]), count(&pair[1]));
                if now >= DOUBLING_MIN_EVENTS && now >= before.saturating_mul(2) {
                    return Some((class.to_owned(), pair[1].clone(), now, before));
                }
            }
        }
        None
    }

    /// Renders the whole report.
    fn render(&self, root: &Path, since: &str, until: &str, scope: Scope) -> String {
        let mut out = String::new();
        let total = self.calls.len();
        let pending: usize = self.pending.values().sum();
        let _ = writeln!(out, "input {}", root.display());
        let _ = writeln!(
            out,
            "window {since}..{until}, scope {scope:?}: {total} terminal tool calls \
             (+{pending} pending replies, excluded) in {} journal keys",
            self.keys
        );
        if self.malformed > 0 {
            let _ = writeln!(
                out,
                "WARNING: {} journal lines were not valid JSON and were skipped; the counts are a lower bound",
                self.malformed
            );
        }
        if self.probes > 0 {
            let _ = writeln!(
                out,
                "{} internal probe lines (the front's actor query) are not agent calls and are excluded",
                self.probes
            );
        }
        out.push('\n');
        let days: Vec<&str> = self
            .calls
            .iter()
            .map(|call| call.day.as_str())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let _ = writeln!(
            out,
            "day         calls   ok  degr  fault  unexpl  IDE%   (known%)  caller  honest  other  pending"
        );
        let row = |label: &str, counts: &BTreeMap<Kind, usize>, calls: usize, pending: usize| {
            let get = |kind: Kind| counts.get(&kind).copied().unwrap_or(0);
            let ide = get(Kind::Fault) + get(Kind::Unexplained);
            format!(
                "{label}  {calls:6} {:6} {:5} {:6} {:6}  {:5.2}% ({:5.2}%)  {:6}  {:6}  {:5}  {pending:6}\n",
                get(Kind::Ok),
                get(Kind::Degraded),
                get(Kind::Fault),
                get(Kind::Unexplained),
                percent(ide, calls),
                percent(get(Kind::Fault), calls),
                get(Kind::Caller),
                get(Kind::Honest),
                get(Kind::Unclassified),
            )
        };
        for day in &days {
            let counts = self.tally(|call| call.day == *day);
            let calls = counts.values().sum();
            out.push_str(&row(
                day,
                &counts,
                calls,
                self.pending.get(*day).copied().unwrap_or(0),
            ));
        }
        let counts = self.tally(|_| true);
        out.push_str(&row("TOTAL     ", &counts, total, pending));
        let _ = writeln!(
            out,
            "IDE% = (fault + unexplained) / terminal calls; known% = fault only; degr = completed through a weaker path"
        );

        let _ = writeln!(out, "\nclass table (share of all calls in the window):");
        let mut by_class: BTreeMap<(Kind, &str), usize> = BTreeMap::new();
        for call in self.calls.iter().filter(|call| !call.class.is_empty()) {
            *by_class
                .entry((call.kind, call.class.as_str()))
                .or_default() += 1;
        }
        for kind in [
            Kind::Fault,
            Kind::Unexplained,
            Kind::Caller,
            Kind::Honest,
            Kind::Unclassified,
        ] {
            let mut rows: Vec<(usize, &str)> = by_class
                .iter()
                .filter(|((k, _), _)| *k == kind)
                .map(|((_, class), count)| (*count, *class))
                .collect();
            if rows.is_empty() {
                continue;
            }
            rows.sort_by(|a, b| b.cmp(a));
            let _ = writeln!(
                out,
                "\n[{}] {} calls",
                kind.tag(),
                rows.iter().map(|row| row.0).sum::<usize>()
            );
            for (count, class) in rows {
                let mark = if matches!(kind, Kind::Fault | Kind::Unexplained)
                    && (count as f64) / (total as f64) > 0.0005
                {
                    " *"
                } else {
                    ""
                };
                let _ = writeln!(
                    out,
                    "  {count:5}  {:5.2}%  {class}{mark}",
                    percent(count, total)
                );
            }
        }
        let _ = writeln!(
            out,
            "\n  * = IDE fault or unexplained class above 0.05% of calls (needs a root cause or an explicit 'unexplained')"
        );

        let _ = writeln!(out, "\nper-day series of fault classes:");
        let _ = writeln!(
            out,
            "  {}  class",
            days.iter()
                .map(|day| day.get(5..).unwrap_or(day))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let mut fault_classes: Vec<(usize, &str, Kind)> = by_class
            .iter()
            .filter(|((kind, _), _)| matches!(kind, Kind::Fault | Kind::Unexplained))
            .map(|((kind, class), count)| (*count, *class, *kind))
            .collect();
        fault_classes.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        for (_, class, kind) in fault_classes {
            let series = days
                .iter()
                .map(|day| {
                    let count = self
                        .calls
                        .iter()
                        .filter(|call| call.day == *day && call.class == class)
                        .count();
                    format!("{count:5}")
                })
                .collect::<Vec<_>>()
                .join(" ");
            let _ = writeln!(
                out,
                "  {series}  {class}{}",
                if kind == Kind::Unexplained {
                    "  (unexplained)"
                } else {
                    ""
                }
            );
        }
        self.render_extras(&mut out);
        out
    }

    /// Renders the by-method, by-repository, outage, settlement and lifecycle sections.
    fn render_extras(&self, out: &mut String) {
        let _ = writeln!(out, "\nby method (calls, IDE faults):");
        let mut methods: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for call in &self.calls {
            let entry = methods.entry(call.method.as_str()).or_default();
            entry.0 += 1;
            entry.1 += usize::from(matches!(call.kind, Kind::Fault | Kind::Unexplained));
        }
        for (method, (calls, faults)) in methods {
            let _ = writeln!(
                out,
                "  {method:8} {calls:6} {faults:5}  {:5.2}%",
                percent(faults, calls)
            );
        }
        let _ = writeln!(out, "\nby repository key, top 5 by IDE faults:");
        let mut keys: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for call in &self.calls {
            let entry = keys.entry(call.key.as_str()).or_default();
            entry.0 += 1;
            entry.1 += usize::from(matches!(call.kind, Kind::Fault | Kind::Unexplained));
        }
        let mut keys: Vec<_> = keys.into_iter().collect();
        keys.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(a.0.cmp(b.0)));
        for (key, (calls, faults)) in keys.into_iter().take(5) {
            let _ = writeln!(
                out,
                "  {key} {calls:6} {faults:5}  {:5.2}%",
                percent(faults, calls)
            );
        }
        match self.longest_outage() {
            Some((count, key, first, last)) => {
                let _ = writeln!(
                    out,
                    "\nlongest outage streak: {count} consecutive IDE-fault calls in {key}, {first} .. {last}"
                );
            }
            None => {
                let _ = writeln!(out, "\nlongest outage streak: none");
            }
        }
        let s = &self.settlement;
        let _ = writeln!(
            out,
            "\npending settlement: {} pending replies; {} collected by ide.inspect; uncollected: \
             {} completed (completion record), {} refused or unknown (completion record), \
             {} failed (job line), {} unknown",
            s.pending,
            s.collected,
            s.uncollected_completed,
            s.uncollected_refused,
            s.uncollected_failed,
            s.uncollected_unknown
        );
        let l = &self.lifecycle;
        let _ = writeln!(
            out,
            "\ndaemon: {} starts, {} client re-establishments, {} forced replacements, failed: {}",
            l.daemon_starts,
            l.client_reestablished,
            l.forced_replacements,
            if l.daemon_failed.is_empty() {
                "none".to_owned()
            } else {
                l.daemon_failed
                    .iter()
                    .map(|(detail, count)| format!("{detail} x{count}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
        let _ = writeln!(
            out,
            "hook warns: {}",
            if l.hook_warns.is_empty() {
                "none".to_owned()
            } else {
                l.hook_warns
                    .iter()
                    .map(|(detail, count)| format!("{detail} x{count}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
    }

    /// The longest run of consecutive IDE-fault calls of one key, as
    /// `(length, key, first ts, last ts)`.
    fn longest_outage(&self) -> Option<(usize, &str, &str, &str)> {
        let mut best: Option<(usize, &str, &str, &str)> = None;
        let mut run: Option<(usize, &str, &str, &str)> = None;
        for call in &self.calls {
            let fault = matches!(call.kind, Kind::Fault | Kind::Unexplained);
            run = match (fault, run) {
                (true, Some((count, key, first, _))) if key == call.key => {
                    Some((count + 1, key, first, call.ts.as_str()))
                }
                (true, _) => Some((1, call.key.as_str(), call.ts.as_str(), call.ts.as_str())),
                (false, _) => None,
            };
            if let Some(current) = run
                && best.is_none_or(|best| current.0 > best.0)
            {
                best = Some(current);
            }
        }
        best
    }
}

/// How one journal line counts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LineKind {
    /// The terminal dispatch line of a tool call.
    Dispatch,
    /// A front transport outcome line (`front:<kind>`).
    Front,
    /// Anything else (job lines, guard lines, lifecycle facts).
    Other,
}

/// Classifies one line as a dispatch line, a front line or neither.
///
/// New lines carry the serving `version`. Older lines are recognised by the original heuristic:
/// a tool method with a duration but no host or worktree, that is a success, a pending reply, an
/// inspection or a failure without a correlation (a worker job-failure line has one).
fn line_kind(record: &Value) -> LineKind {
    let method = text(record, "method");
    if !TOOLS.contains(&method) {
        return LineKind::Other;
    }
    if record.get("version").is_some() {
        return if text(record, "detail").starts_with("front:") {
            LineKind::Front
        } else {
            LineKind::Dispatch
        };
    }
    if record.get("host").is_some()
        || record.get("worktree").is_some()
        || record.get("duration_ms").is_none()
    {
        return LineKind::Other;
    }
    let outcome = text(record, "outcome");
    if outcome == "completed"
        || outcome == "pending"
        || method == "inspect"
        || record.get("correlation").is_none()
    {
        LineKind::Dispatch
    } else {
        LineKind::Other
    }
}

/// Whether a non-pending dispatch line delivered the retained result of a pending job to its
/// caller (an `ide.inspect` collection).
///
/// A line of the older format is an inspection by its method. A current-format line carries the
/// inspection path's own typed evidence, `delivered`: true when the retained result reached the
/// caller whatever it was (a cached failed read is delivered), false for a refused retrieval
/// (stale authority, expired or unknown reference, a call refused before the cache was reached),
/// so no failure prose or method spelling decides it.
fn delivers_result(record: &Value) -> bool {
    if record.get("version").is_none() {
        return text(record, "method") == "inspect";
    }
    record.get("delivered").and_then(Value::as_bool) == Some(true)
}

/// A string field, or `""`.
fn text<'a>(record: &'a Value, field: &str) -> &'a str {
    record.get(field).and_then(Value::as_str).unwrap_or("")
}

/// Share of `part` in `whole`, in percent (0 for an empty whole).
fn percent(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * part as f64 / whole as f64
    }
}

/// Lists the journals under `root` as `(key, files oldest first)`: either `<key>/events.jsonl`
/// directories (rotated generation first) or flat `<key>.jsonl` copies.
fn journal_files(root: &Path) -> Result<Vec<(String, Vec<PathBuf>)>> {
    let mut journals: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for entry in fs::read_dir(root).map_err(|error| format!("{}: {error}", root.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            let files: Vec<PathBuf> = ["events.jsonl.1", "events.jsonl"]
                .iter()
                .map(|name| path.join(name))
                .filter(|file| file.is_file())
                .collect();
            if let (false, Some(key)) = (files.is_empty(), path.file_name()) {
                journals.insert(key.to_string_lossy().into_owned(), files);
            }
        } else if path.extension().is_some_and(|ext| ext == "jsonl")
            && let Some(key) = path.file_stem()
        {
            journals
                .entry(key.to_string_lossy().into_owned())
                .or_insert_with(|| vec![path.clone()]);
        }
    }
    Ok(journals.into_iter().collect())
}

/// Whether a path is a qualification worktree: the gate, matrix and acceptance clones.
fn is_test_path(worktree: &str) -> bool {
    [
        "/private/tmp/",
        "/tmp/",
        "/var/folders/",
        "/private/var/folders/",
    ]
    .iter()
    .any(|prefix| worktree.starts_with(prefix))
        || worktree.contains("acceptance-tmp")
}

/// A key is test traffic when it names worktrees and every one is a qualification path; a key
/// that names none, or mixes field and test worktrees, counts as field.
fn is_test_key(records: &[Value]) -> bool {
    let mut seen = false;
    for worktree in records
        .iter()
        .filter_map(|record| record.get("worktree")?.as_str())
    {
        seen = true;
        if !is_test_path(worktree) {
            return false;
        }
    }
    seen
}

/// The class and kind of one failed front line, from its closed `front:<kind>` detail.
fn classify_front(detail: &str) -> (String, Kind) {
    match detail {
        "front:invalid_parameters" => (
            "caller: invalid parameters (front)".to_owned(),
            Kind::Caller,
        ),
        "front:missing_host_metadata" => (
            "front: host metadata or attachment missing".to_owned(),
            Kind::Fault,
        ),
        other => (
            format!("front: transport {}", other.trim_start_matches("front:")),
            Kind::Fault,
        ),
    }
}

/// The class and kind of one failed dispatch line. A line of the older format (no `version`) is
/// classified by the published rules alone. A current-format line additionally honors its
/// `eligible` flag (a request refused as input is the caller's, whatever reason the refusal
/// carried) and never lets a reason the rules do not know leave the conservative numerator: it is
/// `unexplained`, not `unclassified`.
fn classify_dispatch(record: &Value) -> (String, Kind) {
    let (class, kind) = classify(text(record, "reason"), text(record, "detail"));
    if record.get("version").is_none() {
        return (class, kind);
    }
    if record.get("eligible").and_then(Value::as_bool) == Some(false) {
        return (
            "caller: request refused as input (not eligible)".to_owned(),
            Kind::Caller,
        );
    }
    if kind == Kind::Unclassified {
        return (
            class.replacen("unclassified:", "unexplained (new reason):", 1),
            Kind::Unexplained,
        );
    }
    (class, kind)
}

/// Puts one failed dispatch line in exactly one class by its `reason` and `detail`; the first
/// matching rule wins, so the order is part of the definition. The rules up to the daemon section
/// are the section (b) taxonomy verbatim; the typed-cause rules (QW-6) sit before the generic
/// rules they refine and match only details older journals never wrote.
fn classify(reason: &str, detail: &str) -> (String, Kind) {
    use Kind::{Caller, Fault, Honest, Unexplained};
    let is = |name: &str| reason == name;
    let starts = |prefix: &str| detail.starts_with(prefix);
    let has = |needle: &str| detail.contains(needle);
    let tool_stage = |tools: &[&str], stages: &[&str]| {
        tools.iter().any(|tool| {
            detail
                .strip_prefix(tool)
                .and_then(|rest| rest.strip_prefix(':'))
                .is_some_and(|rest| stages.iter().any(|stage| rest.starts_with(stage)))
        })
    };
    let (class, kind): (&str, Kind) = if is("provider_unavailable") {
        if has("workspace load failed") {
            ("provider: workspace load failed", Fault)
        } else if has("is unavailable and the source outline refused") {
            (
                "provider: server unavailable, lexical outline refused",
                Fault,
            )
        } else if has("request failed") {
            ("provider: request (exchange) failed", Fault)
        } else if has("spawn failed") || has("initialize") || has("transport gone") {
            ("provider: spawn/initialize/transport", Fault)
        } else if starts("symbol:workspace_symbols") {
            ("provider: bare-name workspace_symbols unavailable", Fault)
        } else if detail
            .strip_prefix("no ")
            .is_some_and(|rest| rest.contains(" server serves this file"))
        {
            ("provider: no server for file, lexical refused", Fault)
        } else if detail
            .strip_suffix(":provider_unavailable")
            .is_some_and(|stage| {
                !stage.is_empty() && stage.bytes().all(|byte| byte.is_ascii_lowercase())
            })
        {
            ("provider: stage-less provider_unavailable", Fault)
        } else {
            ("provider: other", Fault)
        }
    } else if is("provider_loading") {
        ("provider: still loading at deadline", Fault)
    } else if is("") && detail == "hooks_not_delivered" {
        ("binding: hooks_not_delivered", Fault)
    } else if is("") && detail == "missing_pre" {
        ("binding: missing_pre", Fault)
    } else if is("") && detail == "recovery_needed" {
        ("binding: recovery_needed", Fault)
    } else if is("") && (detail == "internal_lock" || detail == "worker_unavailable") {
        ("binding: daemon state unavailable (typed)", Fault)
    } else if is("") && detail.is_empty() {
        ("binding: unavailable without cause", Unexplained)
    } else if is("") && (detail == "inactive_binding" || detail == "never_activated") {
        ("binding: activation stopped/never activated", Honest)
    } else if (is("workspace_authority") || is("source_unavailable"))
        && detail == "store:unavailable"
    {
        ("store: unavailable (typed)", Fault)
    } else if is("internal") {
        ("daemon: internal", Fault)
    } else if is("workspace_activation") && starts("start:worktree_unresolved:identity_commit") {
        ("start: durable identity commit failed", Fault)
    } else if is("workspace_activation") && starts("start:worktree_unresolved:store:") {
        ("start: store failure (typed)", Fault)
    } else if is("workspace_activation") {
        ("start: worktree identity unresolved (generic)", Unexplained)
    } else if is("workspace_authority") {
        ("authority: result belongs to an older activation", Fault)
    } else if is("capacity") {
        ("resource: capacity", Fault)
    } else if is("deadline") {
        ("timeout: deadline", Fault)
    } else if is("edit_outcome_unknown") {
        ("timeout: edit outcome unknown", Fault)
    } else if is("execution_profile") {
        ("execution profile refused", Fault)
    } else if is("invalid_detail") && starts("inspect:detail_expired") {
        ("honest: retained result expired", Honest)
    } else if is("invalid_detail")
        && (starts("inspect:detail_unknown") || starts("inspect:detail_mismatch"))
    {
        ("reference: detail_ref never issued/mismatch", Unexplained)
    } else if is("source_unavailable")
        && tool_stage(
            &["diff"],
            &[
                "child_exit",
                "unstable",
                "unsupported_entry",
                "source_unavailable",
            ],
        )
    {
        ("git: diff snapshot failed", Fault)
    } else if is("source_unavailable")
        && tool_stage(
            &["read", "outline", "edit", "context", "graph", "symbol"],
            &["source_unavailable", "observation_failed"],
        )
    {
        ("source: observation failed (store/fs)", Unexplained)
    } else if is("source_unavailable") && starts("diff:not_a_git_repository") {
        ("honest: not a git repository", Honest)
    } else if is("source_unavailable")
        && (starts("inspect:source_stale") || starts("inspect:source_changed"))
    {
        ("honest: source changed since read", Honest)
    } else if is("conflict") && starts("read_only:") {
        ("caller: reader called edit/test", Caller)
    } else if is("conflict") {
        ("honest: worktree/activation held by another actor", Honest)
    } else if is("unsupported_file") {
        ("honest: unsupported file type", Honest)
    } else if is("resolution_unverified") {
        ("honest: TS project resolution unverified", Honest)
    } else if is("cancelled") {
        ("honest: cancelled", Honest)
    } else if is("source_unavailable") && starts("read:line_range") {
        ("caller: line range past end of file", Caller)
    } else if is("source_unavailable") {
        ("caller: source unavailable (other)", Caller)
    } else if is("no_such_file") {
        ("caller: no such file", Caller)
    } else if is("unknown_symbol") {
        ("caller: unknown symbol", Caller)
    } else if is("edit_refused") {
        ("caller: edit refused (ambiguous/syntax/overlap)", Caller)
    } else if is("stale_source") {
        ("caller: stale source reference", Caller)
    } else if is("outside_allowed_roots") {
        ("caller: outside allowed roots", Caller)
    } else if is("invalid_detail") {
        ("caller: invalid detail/run/environment argument", Caller)
    } else if is("conflicting_duplicate") {
        ("caller: conflicting duplicate operation id", Caller)
    } else if is("source_too_large") {
        ("caller: source too large", Caller)
    } else if is("") {
        ("caller: invalid parameters", Caller)
    } else {
        let key: String = format!("{reason}|{detail}").chars().take(60).collect();
        return (format!("unclassified: {key}"), Kind::Unclassified);
    };
    (class.to_owned(), kind)
}

/// Days since 1970-01-01 of a proleptic-Gregorian civil date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let doy = (153 * i64::from((month + 9) % 12) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of a day count since 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe as i64 + era * 400 + i64::from(month <= 2), month, day)
}

/// The current UTC instant as `YYYY-MM-DDTHH:MM:SSZ`.
fn now_rfc3339() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    let rest = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

/// The day (`YYYY-MM-DD`) `days` before the day of `until` (a day or an RFC 3339 instant).
fn day_before(until: &str, days: u64) -> String {
    let parse =
        |range: std::ops::Range<usize>| until.get(range).and_then(|part| part.parse::<i64>().ok());
    let (Some(year), Some(month), Some(day)) = (parse(0..4), parse(5..7), parse(8..10)) else {
        return "0000-00-00".to_owned();
    };
    let (year, month, day) = civil_from_days(
        days_from_civil(year, month as u32, day as u32) - i64::try_from(days).unwrap_or(0),
    );
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `lines` as one flat journal named `key.jsonl` in a fresh directory.
    fn journal(name: &str, key: &str, lines: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-faults-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{key}.jsonl")), lines.join("\n")).unwrap();
        dir
    }

    /// A dispatch line of the section (b) shape.
    fn dispatch(ts: &str, method: &str, outcome: &str, reason: &str, detail: &str) -> String {
        let mut line = format!(
            r#"{{"ts":"{ts}","level":"info","method":"{method}","outcome":"{outcome}","duration_ms":3"#
        );
        if !reason.is_empty() {
            line += &format!(r#","reason":"{reason}""#);
        }
        if !detail.is_empty() {
            line += &format!(r#","detail":"{detail}""#);
        }
        line + "}"
    }

    /// The rules classify the section (b) examples into their published classes and kinds.
    #[test]
    fn classify_follows_the_published_taxonomy() {
        let cases = [
            (
                "provider_unavailable",
                "read:provider_unavailable ext=gamma",
                "provider: other",
                Kind::Fault,
            ),
            (
                "provider_unavailable",
                "outline:provider_unavailable",
                "provider: stage-less provider_unavailable",
                Kind::Fault,
            ),
            (
                "provider_unavailable",
                "symbol:workspace_symbols",
                "provider: bare-name workspace_symbols unavailable",
                Kind::Fault,
            ),
            (
                "provider_unavailable",
                "no rust server serves this file",
                "provider: no server for file, lexical refused",
                Kind::Fault,
            ),
            (
                "",
                "hooks_not_delivered",
                "binding: hooks_not_delivered",
                Kind::Fault,
            ),
            ("", "missing_pre", "binding: missing_pre", Kind::Fault),
            (
                "",
                "",
                "binding: unavailable without cause",
                Kind::Unexplained,
            ),
            (
                "",
                "never_activated",
                "binding: activation stopped/never activated",
                Kind::Honest,
            ),
            (
                "",
                "invalid_parameters",
                "caller: invalid parameters",
                Kind::Caller,
            ),
            (
                "",
                "internal_lock",
                "binding: daemon state unavailable (typed)",
                Kind::Fault,
            ),
            (
                "workspace_activation",
                "start:worktree_unresolved:identity_commit",
                "start: durable identity commit failed",
                Kind::Fault,
            ),
            (
                "workspace_activation",
                "start:worktree_unresolved:store:busy",
                "start: store failure (typed)",
                Kind::Fault,
            ),
            (
                "workspace_activation",
                "start:durable_state: x",
                "start: worktree identity unresolved (generic)",
                Kind::Unexplained,
            ),
            (
                "source_unavailable",
                "read:observation_failed",
                "source: observation failed (store/fs)",
                Kind::Unexplained,
            ),
            (
                "source_unavailable",
                "store:unavailable",
                "store: unavailable (typed)",
                Kind::Fault,
            ),
            (
                "source_unavailable",
                "read:line_range:file has 3 lines",
                "caller: line range past end of file",
                Kind::Caller,
            ),
            (
                "source_unavailable",
                "diff:child_exit:unknown",
                "git: diff snapshot failed",
                Kind::Fault,
            ),
            ("capacity", "store:busy", "resource: capacity", Kind::Fault),
            (
                "invalid_detail",
                "inspect:detail_expired",
                "honest: retained result expired",
                Kind::Honest,
            ),
            (
                "invalid_detail",
                "inspect:detail_unknown",
                "reference: detail_ref never issued/mismatch",
                Kind::Unexplained,
            ),
            (
                "conflict",
                "read_only:ide.edit:x",
                "caller: reader called edit/test",
                Kind::Caller,
            ),
            (
                "brand_new",
                "x",
                "unclassified: brand_new|x",
                Kind::Unclassified,
            ),
        ];
        for (reason, detail, class, kind) in cases {
            assert_eq!(
                classify(reason, detail),
                (class.to_owned(), kind),
                "{reason}|{detail}"
            );
        }
    }

    /// Old-style and new-style dispatch lines, job lines, guard lines and pending replies are told
    /// apart; a front line counts only when no daemon line shares its request id.
    #[test]
    fn dispatch_lines_front_lines_and_duplicates_are_told_apart() {
        let lines = [
            // Old-style success, old-style failure, a job-failure duplicate (has a correlation).
            dispatch("2026-10-01T10:00:00Z", "read", "completed", "", ""),
            dispatch("2026-10-01T10:00:01Z", "read", "unavailable", "", "missing_pre"),
            r#"{"ts":"2026-10-01T10:00:02Z","method":"read","outcome":"failed","reason":"internal","correlation":"r-1","duration_ms":9}"#.to_owned(),
            // A guard line carries host and is not a call.
            r#"{"ts":"2026-10-01T10:00:03Z","method":"read","outcome":"unavailable","host":"claude","duration_ms":1}"#.to_owned(),
            // New-style: a degraded success, a daemon failure, and two front lines (one answered).
            r#"{"ts":"2026-10-01T10:00:04Z","method":"read","outcome":"degraded","version":"1","request":"a","duration_ms":4}"#.to_owned(),
            r#"{"ts":"2026-10-01T10:00:05Z","method":"read","outcome":"failed","reason":"internal","version":"1","request":"b","duration_ms":4}"#.to_owned(),
            r#"{"ts":"2026-10-01T10:00:06Z","method":"read","outcome":"incomplete","detail":"front:timed_out","version":"1","request":"b","duration_ms":4}"#.to_owned(),
            r#"{"ts":"2026-10-01T10:00:07Z","method":"read","outcome":"incomplete","detail":"front:timed_out","version":"1","request":"c","duration_ms":4}"#.to_owned(),
            // Outside the window.
            dispatch("2026-09-01T10:00:00Z", "read", "completed", "", ""),
        ]
        .map(|line| line.to_string());
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let dir = journal("kinds", "00000000000000ab", &refs);
        let report = Report::build(&dir, "2026-10-01", "2026-10-01", Scope::All).unwrap();
        let counts = report.tally(|_| true);
        assert_eq!(
            report.calls.len(),
            5,
            "{:?}",
            report.calls.iter().map(|c| &c.class).collect::<Vec<_>>()
        );
        assert_eq!(counts[&Kind::Ok], 1);
        assert_eq!(counts[&Kind::Degraded], 1);
        // missing_pre, the daemon `internal`, and the unanswered front timeout.
        assert_eq!(counts[&Kind::Fault], 3);
        // The answered front line (request b) is not a second call.
        assert_eq!(
            report
                .calls
                .iter()
                .filter(|c| c.class.starts_with("front:"))
                .count(),
            1
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Field and test keys are separated by worktree location, and a key naming none is field.
    #[test]
    fn scope_separates_qualification_keys() {
        let dir = std::env::temp_dir().join(format!("xtask-faults-scope-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let call = dispatch("2026-10-01T10:00:00Z", "read", "completed", "", "");
        let named = |worktree: &str| {
            format!(
                r#"{{"ts":"2026-10-01T10:00:00Z","method":"feed","outcome":"completed","worktree":"{worktree}"}}"#
            )
        };
        fs::write(
            dir.join("aaaa.jsonl"),
            format!("{}\n{call}\n", named("/private/tmp/clone")),
        )
        .unwrap();
        fs::write(
            dir.join("bbbb.jsonl"),
            format!("{}\n{call}\n", named("/Users/dev/project")),
        )
        .unwrap();
        fs::write(dir.join("cccc.jsonl"), format!("{call}\n")).unwrap();
        let count = |scope| {
            Report::build(&dir, "2026-10-01", "2026-10-01", scope)
                .unwrap()
                .calls
                .len()
        };
        assert_eq!(
            (count(Scope::All), count(Scope::Field), count(Scope::Test)),
            (3, 2, 1)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Pending replies are excluded from the denominator and settle as collected, uncollected
    /// completed, uncollected failed or unknown.
    #[test]
    fn pending_settlement_is_followed_by_correlation() {
        let pending = |n: u32| {
            format!(
                r#"{{"ts":"2026-10-01T10:00:0{n}Z","method":"diff","outcome":"pending","correlation":"d-{n}","duration_ms":8000}}"#
            )
        };
        let lines = [
            pending(1),
            pending(2),
            pending(3),
            pending(4),
            pending(5),
            // d-1 collected; d-2 completed uncollected; d-3 failed uncollected (older format, no
            // call id); d-4 unknown; d-5 failed uncollected (current format, carries the call id).
            r#"{"ts":"2026-10-01T10:00:09Z","method":"inspect","outcome":"completed","correlation":"d-1","duration_ms":3}"#.to_owned(),
            r#"{"ts":"2026-10-01T10:00:10Z","method":"diff","outcome":"completed","detail":"pending_completion","correlation":"d-2"}"#.to_owned(),
            r#"{"ts":"2026-10-01T10:00:11Z","method":"diff","outcome":"failed","reason":"deadline","correlation":"d-3","duration_ms":120000}"#.to_owned(),
            r#"{"ts":"2026-10-01T10:00:12Z","method":"diff","outcome":"failed","reason":"internal","correlation":"d-5","request":"c-5","duration_ms":9}"#.to_owned(),
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let dir = journal("settle", "00000000000000cd", &refs);
        let report = Report::build(&dir, "2026-10-01", "2026-10-01", Scope::All).unwrap();
        let s = &report.settlement;
        assert_eq!(
            (
                s.pending,
                s.collected,
                s.uncollected_completed,
                s.uncollected_failed,
                s.uncollected_unknown
            ),
            (5, 1, 1, 2, 1)
        );
        assert_eq!(report.pending.values().sum::<usize>(), 5);
        // The inspection, the uncollected completion record and the uncollected current-format
        // failure are terminal calls, each counted once; the older-format failure keeps the
        // published dispatch-only count.
        let counts = report.tally(|_| true);
        assert_eq!(
            report.calls.len(),
            3,
            "{:?}",
            report.calls.iter().map(|c| &c.class).collect::<Vec<_>>()
        );
        assert_eq!((counts[&Kind::Ok], counts[&Kind::Fault]), (2, 1));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Collection is the inspection path's own typed delivery evidence, never a method spelling or
    /// failure prose: a settled edit retrieved through `ide.inspect` (journaled under `edit`)
    /// collects its pending job; a cached *failed* read (capacity, `store:busy`) is delivered and
    /// collects it too (no double count with the job's failure line); a refused retrieval — stale
    /// authority, or a host-binding refusal that still names an origin — delivers nothing and
    /// leaves the orphan counted; a completion record that is a refusal is labeled by its outcome.
    #[test]
    fn collection_needs_delivery_evidence_and_completions_are_labeled_by_outcome() {
        let pending = |n: u32, method: &str| {
            format!(
                r#"{{"ts":"2026-10-01T10:00:0{n}Z","method":"{method}","outcome":"pending","correlation":"e-{n}","duration_ms":8000,"version":"1","request":"c-{n}"}}"#
            )
        };
        let lines = [
            pending(1, "edit"),
            pending(2, "read"),
            pending(3, "edit"),
            pending(4, "read"),
            pending(5, "read"),
            // e-1: collected by an inspection journaled under `edit`.
            r#"{"ts":"2026-10-01T10:00:10Z","method":"edit","outcome":"completed","correlation":"e-1","origin":"c-1","delivered":true,"version":"1","request":"i-1","duration_ms":3}"#.to_owned(),
            // e-2: the inspection was refused (stale authority): nothing delivered; the job's own
            // failure line is the call's terminal row.
            r#"{"ts":"2026-10-01T10:00:11Z","method":"inspect","outcome":"failed","reason":"workspace_authority","detail":"inspect:authority_stale","correlation":"e-2","delivered":false,"version":"1","request":"i-2","origin":"c-2","duration_ms":3}"#.to_owned(),
            r#"{"ts":"2026-10-01T10:00:12Z","method":"read","outcome":"failed","reason":"internal","correlation":"e-2","request":"c-2","duration_ms":9}"#.to_owned(),
            // e-3: uncollected, and the completion record is a refusal, not a completion.
            r#"{"ts":"2026-10-01T10:00:13Z","method":"edit","outcome":"invalid","reason":"stale_source","detail":"pending_completion","correlation":"e-3","request":"c-3"}"#.to_owned(),
            // e-4: a cached failed read delivered by the inspection (store:busy): collected; the
            // job's failure line must not be counted again.
            r#"{"ts":"2026-10-01T10:00:14Z","method":"inspect","outcome":"failed","reason":"capacity","detail":"store:busy","correlation":"e-4","delivered":true,"version":"1","request":"i-4","origin":"c-4","duration_ms":3}"#.to_owned(),
            r#"{"ts":"2026-10-01T10:00:15Z","method":"read","outcome":"failed","reason":"capacity","detail":"store:busy","correlation":"e-4","request":"c-4","duration_ms":9}"#.to_owned(),
            // e-5: an inspection refused by host binding (missing_pre) still names its origin but
            // delivered nothing, so the orphan failure is still counted.
            r#"{"ts":"2026-10-01T10:00:16Z","method":"inspect","outcome":"unavailable","detail":"missing_pre","correlation":"e-5","delivered":false,"version":"1","request":"i-5","origin":"c-5","duration_ms":3}"#.to_owned(),
            r#"{"ts":"2026-10-01T10:00:17Z","method":"read","outcome":"failed","reason":"internal","correlation":"e-5","request":"c-5","duration_ms":9}"#.to_owned(),
            // The front's private actor query is not an agent call.
            r#"{"ts":"2026-10-01T10:00:18Z","method":"context","outcome":"unavailable","probe":"whois","version":"1","request":"w-1","duration_ms":1}"#.to_owned(),
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let dir = journal("collect", "00000000000000b2", &refs);
        let report = Report::build(&dir, "2026-10-01", "2026-10-01", Scope::All).unwrap();
        let s = &report.settlement;
        assert_eq!(
            (
                s.pending,
                s.collected,
                s.uncollected_completed,
                s.uncollected_refused,
                s.uncollected_failed,
                s.uncollected_unknown
            ),
            (5, 2, 0, 1, 2, 0)
        );
        assert_eq!(report.probes, 1);
        // Terminal rows, each once: the collecting edit line (e-1), the refused inspection and
        // the orphan failure of e-2, the refused completion of e-3, the delivered failed read of
        // e-4, the refused inspection and the orphan failure of e-5.
        let classes: Vec<&str> = report
            .calls
            .iter()
            .map(|call| call.class.as_str())
            .collect();
        assert_eq!(report.calls.len(), 7, "{classes:?}");
        assert!(
            report
                .render(&dir, "2026-10-01", "2026-10-01", Scope::All)
                .contains("1 internal probe lines")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The outage streak reads chronologically even when an orphan terminal row (journaled earlier
    /// than a later refused inspection but counted after it) is appended last.
    #[test]
    fn the_outage_streak_is_chronological_with_orphan_rows() {
        let lines = [
            r#"{"ts":"2026-10-01T10:00:00Z","method":"read","outcome":"pending","correlation":"o-1","duration_ms":8000,"version":"1","request":"c-1"}"#.to_owned(),
            // The refused inspection comes later in the journal than the orphan job failure it
            // does not collect, but the orphan is counted after it.
            r#"{"ts":"2026-10-01T10:00:02Z","method":"inspect","outcome":"failed","reason":"workspace_authority","detail":"inspect:authority_stale","correlation":"o-1","delivered":false,"version":"1","request":"i-1","origin":"c-1","duration_ms":3}"#.to_owned(),
            r#"{"ts":"2026-10-01T10:00:01Z","method":"read","outcome":"failed","reason":"internal","correlation":"o-1","request":"c-1","duration_ms":9}"#.to_owned(),
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let dir = journal("outage", "00000000000000c3", &refs);
        let report = Report::build(&dir, "2026-10-01", "2026-10-01", Scope::All).unwrap();
        let (count, _, first, last) = report.longest_outage().expect("two consecutive faults");
        assert_eq!(
            (count, first, last),
            (2, "2026-10-01T10:00:01Z", "2026-10-01T10:00:02Z")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The rate alert needs enough calls, the unexplained warning has its own threshold, and a
    /// clean report raises nothing.
    #[test]
    fn alerts_follow_the_thresholds() {
        let mut lines = Vec::new();
        for _ in 0..90 {
            lines.push(dispatch(
                "2026-10-01T10:00:00Z",
                "read",
                "completed",
                "",
                "",
            ));
        }
        for _ in 0..10 {
            lines.push(dispatch(
                "2026-10-01T10:00:01Z",
                "read",
                "unavailable",
                "",
                "missing_pre",
            ));
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let dir = journal("alert", "00000000000000ef", &refs);
        let report = Report::build(&dir, "2026-10-01", "2026-10-01", Scope::All).unwrap();
        let options = |threshold: Option<f64>, min_calls: usize| Options {
            root: None,
            since: None,
            until: None,
            days: DEFAULT_DAYS,
            scope: Scope::All,
            alert_threshold: threshold,
            min_calls,
            unexplained_threshold: DEFAULT_UNEXPLAINED_PERCENT,
        };
        // 10% IDE faults over 100 calls.
        assert_eq!(report.alerts(&options(Some(2.0), 50)).len(), 1);
        assert!(
            report.alerts(&options(Some(2.0), 300)).is_empty(),
            "too few calls"
        );
        assert!(
            report.alerts(&options(Some(20.0), 50)).is_empty(),
            "under the threshold"
        );
        assert!(
            report.alerts(&options(None, 50)).is_empty(),
            "no threshold, no rate alert"
        );
        assert_eq!(report.longest_outage().map(|o| o.0), Some(10));
        let _ = fs::remove_dir_all(&dir);
    }

    /// An unexplained share above its threshold only warns (the command still succeeds), and a
    /// journaled panic is an alert.
    #[test]
    fn unexplained_warns_and_a_panic_alerts() {
        let mut lines = vec![dispatch(
            "2026-10-01T10:00:00Z",
            "read",
            "unavailable",
            "",
            "",
        )];
        lines
            .extend((0..99).map(|_| dispatch("2026-10-01T10:00:01Z", "read", "completed", "", "")));
        lines.push(
            r#"{"ts":"2026-10-01T10:00:02Z","method":"daemon","outcome":"failed","reason":"internal","detail":"panic at crates/x.rs:1:1"}"#
                .to_owned(),
        );
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let dir = journal("warn", "00000000000000f0", &refs);
        let report = Report::build(&dir, "2026-10-01", "2026-10-01", Scope::All).unwrap();
        let options = parse(&[]).unwrap();
        assert_eq!(
            report.warnings(&options).len(),
            1,
            "1% unexplained exceeds 0.1%"
        );
        let alerts = report.alerts(&options);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].contains("panic at crates/x.rs:1:1"), "{alerts:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Dates round-trip and the default window starts the requested days before the end.
    #[test]
    fn windows_use_calendar_days() {
        assert_eq!(day_before("2026-10-08T12:00:00Z", 7), "2026-10-01");
        assert_eq!(day_before("2026-03-01", 1), "2026-02-28");
        assert_eq!(day_before("2024-03-01", 1), "2024-02-29");
        assert_eq!(civil_from_days(days_from_civil(2026, 10, 7)), (2026, 10, 7));
        assert!(parse(&["--scope".into(), "nope".into()]).is_err());
        assert!(parse(&["--bogus".into()]).is_err());
    }

    /// Arguments are validated: a NaN, infinite or out-of-range percentage would silently disable
    /// an alert, and an impossible date or an empty window would silently report nothing.
    #[test]
    fn invalid_arguments_are_refused() {
        let args = |flag: &str, value: &str| parse(&[flag.to_owned(), value.to_owned()]);
        for bad in ["NaN", "inf", "-1", "100.5", "abc", ""] {
            assert!(args("--alert-threshold", bad).is_err(), "{bad}");
            assert!(args("--unexplained-threshold", bad).is_err(), "{bad}");
        }
        assert!(args("--alert-threshold", "2.5").is_ok());
        for bad in [
            "2026-13-01",
            "2026-02-30",
            "2025-02-29",
            "26-10-01",
            "2026/10/01",
            "x",
        ] {
            assert!(args("--since", bad).is_err(), "{bad}");
        }
        assert!(args("--since", "2024-02-29").is_ok());
        for bad in [
            "2026-10-01T25:00:00Z",
            "2026-10-01T10:61:00Z",
            "2026-10-01 10:00:00Z",
            "2026-10-01T10:00:00",
        ] {
            assert!(args("--until", bad).is_err(), "{bad}");
        }
        assert!(args("--until", "2026-10-07T19:30:00Z").is_ok());
        assert!(args("--days", "0").is_err());
        assert!(args("--min-calls", "x").is_err());
    }

    /// A current-format failure honors its `eligible` flag and an unknown reason stays in the
    /// conservative numerator; the older format keeps the published rules (unknown stays
    /// unclassified); a damaged journal line is counted and disclosed, an unreadable one fails.
    #[test]
    fn current_format_lines_honor_eligibility_and_unknown_reasons_and_damage_is_disclosed() {
        let lines = [
            // Old format, unknown reason: unclassified (published rules).
            dispatch("2026-10-01T10:00:00Z", "read", "failed", "brand_new", ""),
            // Current format, unknown reason: unexplained.
            r#"{"ts":"2026-10-01T10:00:01Z","method":"read","outcome":"failed","reason":"restarting","version":"1","eligible":true,"duration_ms":2}"#.to_owned(),
            // Current format, refused as input, whatever reason it carries: the caller's.
            r#"{"ts":"2026-10-01T10:00:02Z","method":"read","outcome":"unavailable","detail":"internal_lock","version":"1","eligible":false,"duration_ms":2}"#.to_owned(),
            "this is not json".to_owned(),
            "{\"truncated\":".to_owned(),
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let dir = journal("current", "00000000000000a1", &refs);
        let report = Report::build(&dir, "2026-10-01", "2026-10-01", Scope::All).unwrap();
        let counts = report.tally(|_| true);
        assert_eq!(
            (
                counts[&Kind::Unclassified],
                counts[&Kind::Unexplained],
                counts[&Kind::Caller]
            ),
            (1, 1, 1)
        );
        assert_eq!(report.malformed, 2);
        assert!(
            report
                .render(&dir, "2026-10-01", "2026-10-01", Scope::All)
                .contains("2 journal lines were not valid JSON")
        );
        // An unreadable journal is an error, not an empty report.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let path = dir.join("00000000000000a1.jsonl");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
            if fs::read(&path).is_err() {
                assert!(Report::build(&dir, "2026-10-01", "2026-10-01", Scope::All).is_err());
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// The frozen section (b) journals (when present) reproduce the published numbers: 16,379
    /// terminal field calls, 692 IDE faults of which 500 known and 192 unexplained, 491 caller
    /// mistakes and 163 honest refusals, 727 pending replies.
    #[test]
    fn frozen_section_b_journals_reproduce_the_published_numbers() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target/stability/journals");
        if !root.is_dir() {
            eprintln!("frozen journals absent; skipping the reproduction check");
            return;
        }
        let report =
            Report::build(&root, "2026-09-28", "2026-10-07T19:30:00Z", Scope::Field).unwrap();
        let counts = report.tally(|_| true);
        let get = |kind: Kind| counts.get(&kind).copied().unwrap_or(0);
        assert_eq!(report.calls.len(), 16_379);
        assert_eq!((get(Kind::Fault), get(Kind::Unexplained)), (500, 192));
        assert_eq!((get(Kind::Caller), get(Kind::Honest)), (491, 163));
        assert_eq!(report.pending.values().sum::<usize>(), 727);
        assert_eq!(get(Kind::Unclassified), 0);
    }
}
