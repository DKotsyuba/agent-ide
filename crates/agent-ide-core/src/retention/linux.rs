//! Conservative, namespace-local Linux process observations from procfs.
//!
//! Even a successful scan cannot prove that shared state has no users in other PID namespaces.
//! Destructive cleanup therefore uses the separate fail-closed global evidence API.

use std::fs;
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::SystemTime;

use super::Process;

/// Returns field 22 of a proc stat record, even when the command contains spaces or parentheses.
fn parse_start_ticks(stat: &str) -> Option<u64> {
    let (_, fields) = stat.rsplit_once(") ")?;
    fields.split_whitespace().nth(19)?.parse().ok()
}

/// Reads the kernel start-tick identity for a process, or refuses missing or malformed evidence.
pub(crate) fn start_ticks(proc_root: &Path, pid: i32) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    parse_start_ticks(&fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?)
}

/// Linux exposes start ticks, but not a stable wall-clock start across clock adjustments. The
/// retention proof compares against filesystem wall-clock mtime, so an uncertain time is unsafe.
pub(super) fn start_time(_proc_root: &Path, _pid: i32) -> Option<SystemTime> {
    None
}

/// Extracts the effective UID (the second Uid field) from a proc status record.
fn effective_uid(status: &str) -> Option<u32> {
    status.lines().find_map(|line| {
        let fields = line.strip_prefix("Uid:")?;
        fields.split_whitespace().nth(1)?.parse().ok()
    })
}

/// Treats a disappeared process as an ordinary exit; any other failed read invalidates the scan.
fn exited(path: &Path, error: &io::Error) -> bool {
    error.kind() == ErrorKind::NotFound
        && fs::symlink_metadata(path).is_err_and(|error| error.kind() == ErrorKind::NotFound)
}

/// Lists visible same-user agent executables, keeping unreadable executable identities as unknown.
/// A changed generation makes this observation unavailable; success still proves no absence in
/// other PID namespaces and must not authorize removal of shared state.
pub(super) fn snapshot(proc_root: &Path) -> Option<Vec<Process>> {
    if proc_root == Path::new("/proc") {
        // A hidepid mount may omit processes from readdir entirely, so it cannot prove absence.
        let mounts = fs::read_to_string("/proc/self/mountinfo").ok()?;
        let mount = mounts.lines().find(|line| {
            let mut fields = line.split_whitespace();
            fields.nth(4) == Some("/proc")
        })?;
        if mount.split_once(" - ")?.1.split_whitespace().next() != Some("proc") {
            return None;
        }
        if mount
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .any(|option| {
                option == "hidepid"
                    || option
                        .strip_prefix("hidepid=")
                        .is_some_and(|value| value != "0")
            })
        {
            return None;
        }
    }
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let mut processes = Vec::new();
    for entry in fs::read_dir(proc_root).ok()? {
        let entry = entry.ok()?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
            .filter(|pid| *pid > 0)
        else {
            continue;
        };
        let dir = entry.path();
        let before = match fs::read_to_string(dir.join("stat")) {
            Ok(stat) => parse_start_ticks(&stat)?,
            Err(error) if exited(&dir, &error) => continue,
            Err(_) => return None,
        };
        let status = match fs::read_to_string(dir.join("status")) {
            Ok(status) => status,
            Err(error) if exited(&dir, &error) => continue,
            Err(_) => return None,
        };
        if effective_uid(&status)? != uid {
            continue;
        }
        let executable = match fs::read_link(dir.join("exe")) {
            Ok(path) => Some(path),
            Err(error) if exited(&dir, &error) => continue,
            Err(_) => None,
        };
        if start_ticks(proc_root, pid) != Some(before) {
            return None;
        }
        if executable.as_ref().is_none_or(|path| {
            path.file_name()
                .is_some_and(|name| name.as_bytes().starts_with(b"agent-ide"))
        }) {
            // A deleted executable cannot be safely matched to the file now at that path.
            let executable =
                executable.filter(|path| !path.as_os_str().as_bytes().ends_with(b" (deleted)"));
            processes.push((pid, executable));
        }
    }
    Some(processes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stat parser handles embedded spaces and closing parentheses and rejects short records.
    #[test]
    fn stat_start_handles_command_names_and_malformed_records() {
        assert_eq!(
            parse_start_ticks("10 (a ) b) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 123 20"),
            Some(123)
        );
        assert_eq!(parse_start_ticks("10 (broken) S 1"), None);
    }

    /// Live and exited processes have distinguishable kernel identities.
    #[test]
    fn current_and_missing_process_start() {
        assert!(start_ticks(Path::new("/proc"), std::process::id() as i32).is_some());
        assert!(start_ticks(Path::new("/proc"), -1).is_none());
        assert!(start_ticks(Path::new("/proc/nonexistent-agent-ide"), 1).is_none());
    }

    /// Incomplete evidence must pause retention, even when the process directory still exists.
    #[test]
    fn inaccessible_or_incomplete_process_evidence_refuses_snapshot() {
        let scratch = crate::scratch::ScratchDir::new("proc-evidence");
        fs::create_dir(scratch.join("123")).unwrap();
        assert!(snapshot(&scratch).is_none());
        fs::write(scratch.join("123/stat"), "malformed").unwrap();
        assert!(snapshot(&scratch).is_none());
    }

    /// A POSIX record lock conflicts with the session guard despite Linux's independent flock.
    #[test]
    fn session_guard_respects_posix_record_locks() {
        use std::os::fd::AsRawFd;
        let scratch = crate::scratch::ScratchDir::new("proc-lock");
        let path = scratch.join("session.lock");
        let file = fs::File::create(&path).unwrap();
        // SAFETY: zero initializes the optional fields of a POSIX whole-file lock.
        let mut record: libc::flock = unsafe { std::mem::zeroed() };
        record.l_type = libc::F_WRLCK as _;
        record.l_whence = libc::SEEK_SET as _;
        // SAFETY: `file` is open and `record` points to an initialized flock.
        assert_eq!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &record) },
            0
        );
        assert!(super::super::lock_session(&path, true).is_none());
        record.l_type = libc::F_UNLCK as _;
        // SAFETY: this releases the test's own record lock using the same live descriptor.
        assert_eq!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &record) },
            0
        );
        assert!(super::super::lock_session(&path, true).is_some());
    }
}
