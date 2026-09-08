//! Contract checks for Workspace raw Git parsing and comparison provenance.

use std::os::unix::ffi::OsStrExt;

use agent_ide::workspace::git::{
    BaselineContext, BaselineCoverage, StatusKind, parse_porcelain_v2_z, parse_terminal_path,
};

/// Preserves terminal-LF discovery output even when the Unix path itself contains a newline byte.
#[test]
fn terminal_discovery_removes_only_the_final_lf() {
    let path = parse_terminal_path(b"/private/tmp/line\nbreak\n").expect("terminal LF is present");
    assert_eq!(path.as_os_str().as_bytes(), b"/private/tmp/line\nbreak");
    assert!(parse_terminal_path(b"/private/tmp/no-delimiter").is_err());
}

/// Keeps NUL-safe tracked, conflict, rename, and untracked paths in distinct result groups.
#[test]
fn porcelain_v2_preserves_raw_paths_and_separate_untracked() {
    assert!(parse_porcelain_v2_z(b"? unterminated").is_err());
    let raw = b"1 M. N... 100644 100644 100644 a b - spaced name\0u UU N... 100644 100644 100644 100644 a b c conflict\0? -leading\npath\x002 R. N... 100644 100644 100644 a b R100 renamed\0old name\0";
    let status = parse_porcelain_v2_z(raw).expect("fixed porcelain records parse");
    assert_eq!(status.tracked().len(), 2);
    assert_eq!(status.conflicts().len(), 1);
    assert_eq!(status.untracked().len(), 1);
    assert_eq!(status.untracked()[0].kind(), StatusKind::Untracked);
    assert_eq!(
        status.untracked()[0].path().as_os_str().as_bytes(),
        b"-leading\npath"
    );
    assert_eq!(
        status.tracked()[1]
            .original_path()
            .expect("rename has an original path")
            .as_os_str()
            .as_bytes(),
        b"old name"
    );
    assert_eq!(status.tracked()[1].kind(), StatusKind::RenamedOrCopied);
    assert_eq!(
        status.tracked()[1].path().as_os_str().as_bytes(),
        b"renamed"
    );
}

/// Retains explicit partial baseline provenance without inventing complete evidence.
#[test]
fn baseline_context_preserves_partial_coverage() {
    let baseline = BaselineContext::new("session-baseline", BaselineCoverage::Partial).unwrap();
    assert_eq!(baseline.reference(), "session-baseline");
    assert_eq!(baseline.coverage(), BaselineCoverage::Partial);
}
