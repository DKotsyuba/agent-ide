//! Resolves the real per-user home from the password database, never from `$HOME`.
//!
//! Hosts such as `agent-run` start MCP clients, daemons and shells with a substitute `HOME`. Every
//! per-user state root (error log, check caches, telemetry) and every toolchain home (cargo,
//! rustup) must still land in the owner's real home, so this is the one place they are derived.
//! `AGENT_IDE_HOME` is the only override and exists for tests and explicit relocation.

use std::{
    ffi::{CStr, OsStr, OsString},
    os::unix::ffi::OsStrExt,
    path::PathBuf,
};

/// Explicit absolute-path override for the resolved home; production leaves it unset.
pub const HOME_OVERRIDE_ENV: &str = "AGENT_IDE_HOME";

/// Returns the effective home: an absolute [`HOME_OVERRIDE_ENV`] when set, else [`passwd_home`].
///
/// `$HOME` is deliberately ignored.
pub fn user_home() -> Option<PathBuf> {
    home_from(std::env::var_os(HOME_OVERRIDE_ENV))
}

/// Applies the override rule to one already-read override value.
fn home_from(override_value: Option<OsString>) -> Option<PathBuf> {
    override_value
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(passwd_home)
}

/// Returns the absolute `pw_dir` of the current real uid from `getpwuid_r`, or `None` when the
/// entry is missing or not absolute.
pub fn passwd_home() -> Option<PathBuf> {
    // SAFETY: `passwd` is plain data; `getpwuid_r` fills it and `buffer` outlives every use of the
    // pointers it stores, and `pw_dir` is read only while both are alive.
    unsafe {
        let mut length = match libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) {
            size if size > 0 => size as usize,
            _ => 1024,
        };
        loop {
            let mut buffer = vec![0u8; length];
            let mut entry: libc::passwd = std::mem::zeroed();
            let mut found: *mut libc::passwd = std::ptr::null_mut();
            let status = libc::getpwuid_r(
                libc::getuid(),
                &mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            );
            if status == libc::ERANGE && length < 1 << 20 {
                length *= 2;
                continue;
            }
            if status != 0 || found.is_null() || entry.pw_dir.is_null() {
                return None;
            }
            let path = PathBuf::from(OsStr::from_bytes(CStr::from_ptr(entry.pw_dir).to_bytes()));
            return path.is_absolute().then_some(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwd_home_ignores_a_substituted_home_variable() {
        // SAFETY: this is the only test in the lib that reads or writes `HOME`.
        unsafe { std::env::set_var("HOME", "/nonexistent") };
        let home = passwd_home().expect("the test user has a passwd entry");
        assert!(home.is_absolute());
        assert_ne!(home, PathBuf::from("/nonexistent"));
    }

    #[test]
    fn override_wins_only_when_absolute() {
        assert_eq!(
            home_from(Some("/tmp/fixture-home".into())),
            Some(PathBuf::from("/tmp/fixture-home"))
        );
        assert_eq!(home_from(Some("relative".into())), passwd_home());
        assert_eq!(home_from(None), passwd_home());
    }
}
