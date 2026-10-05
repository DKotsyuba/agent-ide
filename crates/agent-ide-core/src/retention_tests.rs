//! Policy, claim and lock tests for [`super`], all on private temporary state roots.

use super::*;
use std::cell::Cell;

/// One day.
const DAY: Duration = Duration::from_secs(86_400);
/// Repository-level directory name used for every fixture check cache.
const REPO: &str = "0123456789abcdef";

/// Creates a fresh canonical scratch directory unique to this process and `name`.
fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("agent-ide-retention-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    // A state root must not be group or world writable, whatever the umask.
    fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    fs::canonicalize(dir).unwrap()
}

/// Builds `checks/<REPO>/<worktree key>` for `worktree` with a small target file and marker.
fn check_cache(home: &Path, worktree: &Path) -> PathBuf {
    let dir = home.join("checks").join(REPO).join(worktree_key(worktree));
    fs::create_dir_all(dir.join("digest/lang/target")).unwrap();
    fs::write(dir.join("digest/lang/target/artifact"), vec![7u8; 8192]).unwrap();
    fs::write(dir.join(MARKER_FILE_NAME), worktree.as_os_str().as_bytes()).unwrap();
    dir
}

/// Sets the mtime of every entry below `root` to `age` ago.
fn backdate(root: &Path, age: Duration) {
    let when = SystemTime::now() - age;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            stack.extend(fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
        }
        File::open(&path).unwrap().set_modified(when).unwrap();
    }
}

/// A process snapshot with no live processes.
fn nobody() -> Option<Vec<Process>> {
    Some(Vec::new())
}

/// Returns the verdict for `path`, panicking when the policy did not select it.
fn verdict<'a>(report: &'a Report, path: &Path) -> &'a Verdict {
    report
        .verdicts
        .iter()
        .find(|verdict| verdict.path == path)
        .unwrap_or_else(|| panic!("{} not selected: {report:?}", path.display()))
}

/// Sweeps until `path` is no longer in use and returns its fate. A lock just released stays held
/// while a process another test is spawning still has the inherited descriptor between its fork
/// and exec (milliseconds under a parallel run); the product sweep simply waits for its next hour.
fn sweep_released(home: &Path, at: SystemTime, path: &Path) -> Fate {
    for _ in 0..300 {
        let fate = verdict(&sweep_with(home, true, at, &nobody), path).fate;
        if fate != Fate::InUse {
            return fate;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("{} stayed in use for 3 s after its release", path.display())
}

/// Builds a synthetic entry for the pure policy.
fn synthetic(name: &str, bytes: u64, age_days: u64) -> Entry {
    Entry {
        kind: Kind::Checks,
        path: PathBuf::from(format!("/state/checks/{name}")),
        worktree: None,
        lease_keys: Vec::new(),
        bytes,
        last_used: SystemTime::UNIX_EPOCH + DAY * 100 - DAY * age_days as u32,
        gone: false,
        complete: true,
    }
}

/// Gone and idle entries go first, then the least recently used until the family fits its budget.
#[test]
fn select_removes_gone_and_idle_first_then_least_recently_used_until_under_budget() {
    let now = SystemTime::UNIX_EPOCH + DAY * 100;
    let mut gone = synthetic("gone", 10, 0);
    gone.gone = true;
    let entries = vec![
        synthetic("newest", 40, 1),
        synthetic("idle", 30, 9),
        synthetic("older", 40, 3),
        synthetic("middle", 40, 2),
        gone,
    ];
    let mut claimed = Vec::new();
    let verdicts = select(entries, DAY * 7, Some(70), now, &mut |entry| {
        claimed.push(entry.path.clone());
        (Fate::Removed, entry.bytes)
    });
    let decided: Vec<_> = verdicts
        .iter()
        .map(|v| (v.path.file_name().unwrap().to_str().unwrap(), v.reason))
        .collect();
    // 160 bytes: idle (30) and gone (10) leave 120; LRU "older" (40) leaves 80, "middle" 40.
    assert_eq!(
        decided,
        [
            ("idle", Reason::Idle),
            ("gone", Reason::Gone),
            ("older", Reason::Budget),
            ("middle", Reason::Budget),
        ]
    );
    assert_eq!(claimed.len(), 4);
}

/// An in-use entry is kept and the budget is met with the next oldest instead.
#[test]
fn select_skips_in_use_entries_and_evicts_the_next_oldest_instead() {
    let now = SystemTime::UNIX_EPOCH + DAY * 100;
    let entries = vec![
        synthetic("busy", 50, 3),
        synthetic("next", 50, 2),
        synthetic("newest", 50, 1),
    ];
    let verdicts = select(entries, DAY * 7, Some(100), now, &mut |entry| {
        if entry.path.ends_with("busy") {
            (Fate::InUse, entry.bytes)
        } else {
            (Fate::Removed, entry.bytes)
        }
    });
    let fates: Vec<_> = verdicts.iter().map(|v| v.fate).collect();
    assert_eq!(fates, [Fate::InUse, Fate::Removed]);
    assert!(verdicts[1].path.ends_with("next"));
}

/// A paused sweep reports only the budget candidates it would remove, not every entry.
#[test]
fn a_paused_budget_selection_stops_once_the_would_remove_total_fits() {
    let now = SystemTime::UNIX_EPOCH + DAY * 100;
    let entries = vec![
        synthetic("oldest", 50, 3),
        synthetic("middle", 50, 2),
        synthetic("newest", 50, 1),
    ];
    let verdicts = select(entries, DAY * 7, Some(100), now, &mut |entry| {
        (Fate::Paused, entry.bytes)
    });
    assert_eq!(verdicts.len(), 1, "{verdicts:?}");
    assert!(verdicts[0].path.ends_with("oldest"));
    assert_eq!(
        (verdicts[0].reason, verdicts[0].fate),
        (Reason::Budget, Fate::Paused)
    );
}

/// An entry whose tree could not be fully read is never claimed.
#[test]
fn select_never_claims_an_unreadable_entry() {
    let now = SystemTime::UNIX_EPOCH + DAY * 100;
    let mut unreadable = synthetic("unreadable", 10, 30);
    unreadable.complete = false;
    let verdicts = select(vec![unreadable], DAY * 7, Some(0), now, &mut |_| {
        panic!("an unreadable entry must not be claimed")
    });
    assert_eq!(verdicts[0].fate, Fate::Unreadable);
}

/// A gone worktree's cache is removed through the trash; a live, recent one is not selected.
#[test]
fn sweep_removes_a_gone_worktree_cache_and_keeps_a_live_recent_one() {
    let home = scratch("gone");
    let live = scratch("gone-live-worktree");
    let missing = home.join("deleted-worktree");
    let kept = check_cache(&home, &live);
    let removed = check_cache(&home, &missing);

    let report = sweep_with(&home, true, SystemTime::now(), &nobody);

    assert!(!report.paused());
    assert_eq!(verdict(&report, &removed).reason, Reason::Gone);
    assert_eq!(verdict(&report, &removed).fate, Fate::Removed);
    assert!(verdict(&report, &removed).bytes >= 8192);
    assert!(!removed.exists());
    assert!(kept.exists(), "a live, recent cache is not selected");
    assert!(
        fs::read_dir(home.join("checks/.trash"))
            .unwrap()
            .next()
            .is_none(),
        "the claimed tree is deleted from the trash"
    );
}

/// A cache without a marker (a legacy directory) is never judged gone.
#[test]
fn a_cache_without_a_marker_is_never_judged_gone() {
    let home = scratch("unreadable-marker");
    let dir = check_cache(&home, &home.join("deleted"));
    fs::remove_file(dir.join(MARKER_FILE_NAME)).unwrap();
    let report = sweep_with(&home, true, SystemTime::now(), &nobody);
    assert!(report.verdicts.is_empty(), "{report:?}");
    assert!(dir.exists());
}

/// A fresh cache with an unreadable subtree is not selected, but the family total says it is a
/// lower bound.
#[test]
fn an_unreadable_tree_marks_the_family_total_as_a_lower_bound() {
    use std::os::unix::fs::PermissionsExt;
    let home = scratch("unreadable-tree");
    let worktree = scratch("unreadable-tree-worktree");
    let dir = check_cache(&home, &worktree);
    let hidden = dir.join("digest/lang/target");
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
    let report = sweep_with(&home, false, SystemTime::now(), &nobody);
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(report.verdicts.is_empty(), "{report:?}");
    assert!(report.render(false).contains("total is a lower bound"));
}

/// An unreadable marker keeps the cache even when it is long idle.
#[test]
fn an_unreadable_marker_protects_even_an_aged_cache() {
    use std::os::unix::fs::PermissionsExt;
    let home = scratch("marker-000");
    let worktree = scratch("marker-000-worktree");
    let dir = check_cache(&home, &worktree);
    let marker = dir.join(MARKER_FILE_NAME);
    backdate(&dir, DAY * 30);
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o000)).unwrap();
    let report = sweep_with(&home, true, SystemTime::now() + DAY * 30, &nobody);
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(verdict(&report, &dir).fate, Fate::Unreadable);
    assert!(dir.exists());
}

/// A telemetry store's lease keys cover its launch directory and every ancestor, lease file or not.
#[test]
fn telemetry_lease_keys_cover_every_ancestor_even_without_a_lease_file_yet() {
    let home = scratch("telemetry-keys");
    let store = home.join("telemetry").join("c".repeat(64));
    fs::create_dir_all(&store).unwrap();
    let launch = Path::new("/repo/worktree/subdir");
    fs::write(store.join(MARKER_FILE_NAME), launch.as_os_str().as_bytes()).unwrap();
    let (entries, complete) = scan_telemetry(&home.join("telemetry"), &home.join("locks"));
    assert!(complete);
    for ancestor in ["/repo/worktree/subdir", "/repo/worktree", "/repo", "/"] {
        assert!(
            entries[0]
                .lease_keys
                .contains(&worktree_key(Path::new(ancestor))),
            "{ancestor}"
        );
    }
}

/// A cache unused for longer than the idle age is removed; a fresh one is kept.
#[test]
fn sweep_removes_idle_caches_by_age() {
    let home = scratch("idle");
    let worktree = scratch("idle-worktree");
    let idle = check_cache(&home, &worktree);
    backdate(&idle, DAY * 8);
    let fresh_worktree = scratch("idle-fresh-worktree");
    let fresh = check_cache(&home, &fresh_worktree);

    let report = sweep_with(&home, true, SystemTime::now(), &nobody);

    assert_eq!(verdict(&report, &idle).reason, Reason::Idle);
    assert!(!idle.exists());
    assert!(fresh.exists());
}

/// A held lease keeps an idle cache; after release it is removed.
#[test]
fn sweep_never_claims_a_cache_whose_lease_is_held() {
    let home = scratch("leased");
    let worktree = scratch("leased-worktree");
    let dir = check_cache(&home, &worktree);
    backdate(&dir, DAY * 30);
    let lease = Lease::acquire(&home, &worktree).expect("lease");
    // Taking the lease touched it; evaluate as if the idle age had passed anyway.
    let later = SystemTime::now() + DAY * 8;

    let report = sweep_with(&home, true, later, &nobody);
    assert_eq!(verdict(&report, &dir).fate, Fate::InUse);
    assert!(dir.exists());

    drop(lease);
    assert_eq!(sweep_released(&home, later, &dir), Fate::Removed);
    assert!(!dir.exists());
}

/// A lease dropped unsettled stays held; a settled one is released.
#[test]
fn an_unsettled_lease_is_kept_for_the_process_lifetime() {
    let home = scratch("unsettled");
    let worktree = scratch("unsettled-worktree");
    let dir = check_cache(&home, &worktree);
    drop(SettledLease::new(Lease::acquire(&home, &worktree)));
    let report = sweep_with(&home, true, SystemTime::now() + DAY * 30, &nobody);
    assert_eq!(verdict(&report, &dir).fate, Fate::InUse);

    let worktree = scratch("settled-worktree");
    let dir = check_cache(&home, &worktree);
    SettledLease::new(Lease::acquire(&home, &worktree)).settled();
    let report = sweep_with(&home, true, SystemTime::now() + DAY * 30, &nobody);
    assert_eq!(verdict(&report, &dir).fate, Fate::Removed);
}

/// A dry run reports what it would remove and removes nothing.
#[test]
fn a_dry_run_reports_without_removing() {
    let home = scratch("dry");
    let dir = check_cache(&home, &home.join("deleted"));
    let report = sweep_with(&home, false, SystemTime::now(), &nobody);
    assert_eq!(verdict(&report, &dir).fate, Fate::Removed);
    assert!(dir.exists());
    assert!(report.render(false).contains("would remove: checks gone"));
}

/// A legacy or unknown process pauses every check and telemetry removal, gone ones included.
#[test]
fn legacy_or_unknown_processes_pause_all_check_and_telemetry_removal() {
    let home = scratch("legacy");
    let dir = check_cache(&home, &home.join("deleted"));
    let legacy = || {
        Some(vec![(
            42,
            Some(PathBuf::from("/old/releases/0.9.1/agent-ide")),
        )])
    };

    let report = sweep_with(&home, true, SystemTime::now(), &legacy);
    assert!(report.paused());
    assert_eq!(verdict(&report, &dir).fate, Fate::Paused);
    assert!(
        dir.exists(),
        "even a gone worktree waits for legacy processes"
    );
    assert!(report.render(true).contains("pid 42"));

    let report = sweep_with(&home, true, SystemTime::now(), &|| None);
    assert!(report.paused());
    assert!(
        dir.exists(),
        "an unreadable process list protects everything"
    );
}

/// Only this binary and releases strictly newer than the floor take leases; everything else is legacy.
#[test]
fn only_this_binary_and_strictly_newer_releases_participate() {
    let releases = PathBuf::from("/home/.agent-ide/standalone/releases");
    let own = std::env::current_exe().unwrap();
    let identity = Identity {
        own: file_id(&own),
        releases: releases.clone(),
        floor: (0, 9, 1),
    };
    let processes = vec![
        (1, Some(own.clone())),
        (2, Some(releases.join("0.9.2/agent-ide"))),
        (3, Some(releases.join("0.9.1/agent-ide"))),
        (4, Some(releases.join("0.10.0/agent-ide"))),
        (5, Some(PathBuf::from("/work/target/debug/agent-ide"))),
        (
            6,
            Some(PathBuf::from("/Users/x/.local/bin/agent-ide.bak-0.3.1")),
        ),
        (7, Some(PathBuf::from("/usr/bin/ssh"))),
        (8, Some(releases.join("0.9.2/tools/agent-ide"))),
        (9, None),
    ];
    let legacy: Vec<_> = identity
        .legacy(&processes)
        .into_iter()
        .map(|(pid, _)| pid)
        .collect();
    // The test binary itself is not named agent-ide, so pid 1 never counts either way.
    assert_eq!(
        legacy,
        [3, 5, 6, 8, 9],
        "an unreadable executable is legacy"
    );
}

/// The participation floor is at least this build's version and the legacy boundary.
#[test]
fn the_floor_is_never_below_this_build_or_the_legacy_boundary() {
    let identity = Identity::current(&scratch("floor"));
    assert!(identity.floor >= LEGACY_BOUNDARY);
    assert!(identity.floor >= parse_version(env!("CARGO_PKG_VERSION")).unwrap());
}

/// A gone telemetry store is kept while its writer lock is held and removed after.
#[test]
fn telemetry_stores_follow_their_writer_lock_and_marker() {
    let home = scratch("telemetry");
    let store = home.join("telemetry").join("a".repeat(64));
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("state.sqlite"), vec![1u8; 4096]).unwrap();
    fs::write(store.join(MARKER_FILE_NAME), b"/nonexistent/launch-dir").unwrap();
    let writer = try_exclusive(&store.join(TELEMETRY_LOCK)).expect("writer lock");

    let report = sweep_with(&home, true, SystemTime::now(), &nobody);
    assert_eq!(verdict(&report, &store).reason, Reason::Gone);
    assert_eq!(verdict(&report, &store).fate, Fate::InUse);
    assert!(store.exists());

    drop(writer);
    assert_eq!(
        sweep_released(&home, SystemTime::now(), &store),
        Fate::Removed
    );
    assert!(!store.exists());
}

/// A telemetry store launched below an activated worktree is kept until the activation ends.
#[test]
fn a_telemetry_store_below_an_activated_worktree_is_kept() {
    let home = scratch("telemetry-ancestor");
    let worktree = scratch("telemetry-ancestor-worktree");
    let launch = worktree.join("subdir");
    fs::create_dir_all(&launch).unwrap();
    let store = home.join("telemetry").join("b".repeat(64));
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join(MARKER_FILE_NAME), launch.as_os_str().as_bytes()).unwrap();
    let activation = Lease::acquire(&home, &worktree).expect("worktree lease");
    let report = sweep_with(&home, true, SystemTime::now() + DAY * 60, &nobody);
    assert_eq!(verdict(&report, &store).fate, Fate::InUse);
    assert!(store.exists());
    drop(activation);
    assert_eq!(
        sweep_released(&home, SystemTime::now() + DAY * 60, &store),
        Fate::Removed
    );
}

/// A worktree lease taken after the scan still keeps a telemetry store launched below it.
#[test]
fn an_activation_after_the_scan_still_keeps_a_telemetry_store() {
    let home = scratch("telemetry-after-scan");
    let worktree = scratch("telemetry-after-scan-worktree");
    let launch = worktree.join("subdir");
    fs::create_dir_all(&launch).unwrap();
    let store = home.join("telemetry").join("e".repeat(64));
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join(MARKER_FILE_NAME), launch.as_os_str().as_bytes()).unwrap();
    let locks = home.join(LOCKS_DIR);
    let (entries, _) = scan_telemetry(&home.join("telemetry"), &locks);
    let activation = Lease::acquire(&home, &worktree).expect("worktree lease");
    let (fate, _) = claim_entry(&entries[0], &locks, SystemTime::now(), true, &|| true);
    assert_eq!(fate, Fate::InUse);
    assert!(store.exists());
    drop(activation);
}

/// A dry run's lock probes are not uses: an idle cache it reports is still removed right after.
#[test]
fn a_dry_run_does_not_refresh_an_idle_cache() {
    let home = scratch("dry-then-apply");
    let worktree = scratch("dry-then-apply-worktree");
    let dir = check_cache(&home, &worktree);
    backdate(&dir, DAY * 8);
    let report = sweep_with(&home, false, SystemTime::now(), &nobody);
    assert_eq!(verdict(&report, &dir).fate, Fate::Removed);
    assert!(dir.exists());
    let report = sweep_with(&home, true, SystemTime::now(), &nobody);
    assert_eq!(verdict(&report, &dir).fate, Fate::Removed);
    assert!(!dir.exists());
}

/// A telemetry store without a marker is claimable only while no lease is held anywhere.
#[test]
fn a_telemetry_store_without_a_marker_waits_until_no_lease_is_held_anywhere() {
    let home = scratch("telemetry-unmarked");
    let store = home.join("telemetry").join("d".repeat(64));
    fs::create_dir_all(&store).unwrap();
    let elsewhere = Lease::acquire(&home, &scratch("telemetry-unmarked-other")).expect("lease");
    let report = sweep_with(&home, true, SystemTime::now() + DAY * 60, &nobody);
    assert_eq!(verdict(&report, &store).fate, Fate::InUse);
    drop(elsewhere);
    assert_eq!(
        sweep_released(&home, SystemTime::now() + DAY * 60, &store),
        Fate::Removed
    );
}

/// Symlinked cache roots and entries are never traversed or removed.
#[test]
fn symlinked_cache_roots_and_entries_are_never_traversed() {
    let home = scratch("symlink");
    let outside = scratch("symlink-outside");
    let victim = check_cache(&outside, &outside.join("deleted"));
    let trash_victim = outside.join("checks/.trash/leftover");
    fs::create_dir_all(&trash_victim).unwrap();
    std::os::unix::fs::symlink(outside.join("checks"), home.join("checks")).unwrap();
    let report = sweep_with(&home, true, SystemTime::now(), &nobody);
    assert!(report.verdicts.is_empty());
    assert!(victim.exists());
    assert!(
        trash_victim.exists(),
        "trash below a symlinked root is never emptied"
    );

    let home = scratch("symlink-entry");
    fs::create_dir_all(home.join("checks").join(REPO)).unwrap();
    std::os::unix::fs::symlink(
        &victim,
        home.join("checks").join(REPO).join("fedcba9876543210"),
    )
    .unwrap();
    let report = sweep_with(&home, true, SystemTime::now(), &nobody);
    assert!(report.verdicts.is_empty());
    assert!(victim.exists());
}

/// Lays out `standalone/releases/<version>` with `COMPLETE` installed `age` ago.
fn release(standalone: &Path, version: &str, age: Duration, complete: bool) -> PathBuf {
    let dir = standalone.join("releases").join(version);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("agent-ide"), b"binary").unwrap();
    if complete {
        fs::write(dir.join("COMPLETE"), b"sealed").unwrap();
        File::open(dir.join("COMPLETE"))
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }
    dir
}

/// Releases keep current, the newest three, recent and live ones; only the rest are removed.
#[test]
fn releases_keep_current_newest_recent_and_live_and_remove_the_rest() {
    let home = scratch("releases");
    let standalone = home.join("standalone");
    let old = DAY * 30;
    let current = release(&standalone, "0.6.0", old, true);
    std::os::unix::fs::symlink("releases/0.6.0", standalone.join("current")).unwrap();
    let live = release(&standalone, "0.7.0", old, true);
    let superseded = release(&standalone, "0.5.0", old, true);
    let recent = release(&standalone, "0.4.0", DAY, true);
    let incomplete = release(&standalone, "0.3.0", old, false);
    let newest: Vec<_> = ["0.8.0", "0.9.0", "0.9.1"]
        .iter()
        .map(|version| release(&standalone, version, old, true))
        .collect();
    let live_exe = live.join("agent-ide");
    let snapshot = move || Some(vec![(9, Some(live_exe.clone()))]);

    let report = sweep_with(&home, true, SystemTime::now(), &snapshot);

    let removed: Vec<_> = report
        .verdicts
        .iter()
        .filter(|verdict| verdict.kind == Kind::Release)
        .map(|verdict| (verdict.path.clone(), verdict.fate))
        .collect();
    assert_eq!(removed, [(superseded.clone(), Fate::Removed)]);
    assert!(!superseded.exists());
    for kept in [&current, &live, &recent, &incomplete]
        .into_iter()
        .chain(&newest)
    {
        assert!(kept.exists(), "{} must be kept", kept.display());
    }
    assert_eq!(report.releases.entries, 7);
}

/// A release seen live after quarantine is restored, and an unknown snapshot keeps every release.
#[test]
fn a_release_seen_live_after_quarantine_is_restored() {
    let home = scratch("quarantine");
    let standalone = home.join("standalone");
    for version in ["0.9.1", "0.9.0", "0.8.0"] {
        release(&standalone, version, DAY * 30, true);
    }
    let candidate = release(&standalone, "0.5.0", DAY * 30, true);
    let calls = Cell::new(0);
    let quarantined = standalone
        .join("releases")
        .join(format!(".trash-0.5.0-{}", std::process::id()))
        .join("agent-ide");
    // Calls: legacy gate, first release snapshot, then the recheck after the rename.
    let snapshot = || {
        calls.set(calls.get() + 1);
        Some(if calls.get() >= 3 {
            vec![(11, Some(quarantined.clone()))]
        } else {
            Vec::new()
        })
    };
    let report = sweep_with(&home, true, SystemTime::now(), &snapshot);
    assert_eq!(verdict(&report, &candidate).fate, Fate::InUse);
    assert!(
        candidate.join("agent-ide").exists(),
        "restored from quarantine"
    );

    let report = sweep_with(&home, true, SystemTime::now(), &|| None);
    assert!(
        report.verdicts.iter().all(|v| v.kind != Kind::Release),
        "an unreadable process list keeps every release"
    );
    assert!(candidate.exists());
}

/// Sweeps exclude each other and a scheduled one is skipped right after a completed one.
#[test]
fn sweeps_are_serialized_and_spaced() {
    let home = scratch("sweep-lock");
    let first = SweepLock::try_acquire(&home, Some(SWEEP_SPACING)).expect("first sweep");
    assert!(
        SweepLock::try_acquire(&home, None).is_none(),
        "a second sweeper waits for the running one"
    );
    drop(first);
    assert!(
        SweepLock::try_acquire(&home, Some(SWEEP_SPACING)).is_none(),
        "a scheduled sweep right after a completed one is skipped"
    );
    assert!(
        SweepLock::try_acquire(&home, None).is_some(),
        "an explicit prune ignores the spacing"
    );
}
