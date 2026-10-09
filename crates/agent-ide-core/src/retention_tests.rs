//! Policy, claim and lock tests for [`super`], all on private temporary state roots.

use super::*;
use std::cell::Cell;

/// One day.
const DAY: Duration = Duration::from_secs(86_400);
/// Repository-level directory name used for every fixture check cache.
const REPO: &str = "0123456789abcdef";

/// Creates a fresh canonical scratch directory that removes itself, and everything the test put in
/// it, when dropped. `name` only labels it.
fn scratch(name: &str) -> crate::scratch::ScratchDir {
    crate::scratch::ScratchDir::new(&format!("retention-{name}"))
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
        usage: Usage {
            own: bytes,
            ..Usage::default()
        },
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
    let verdicts = select(
        entries,
        DAY * 7,
        Some(70),
        now,
        &mut |entry, freed| {
            claimed.push(entry.path.clone());
            (Fate::Removed, freed)
        },
        None,
    );
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
    let verdicts = select(
        entries,
        DAY * 7,
        Some(100),
        now,
        &mut |entry, freed| {
            if entry.path.ends_with("busy") {
                (Fate::InUse, freed)
            } else {
                (Fate::Removed, freed)
            }
        },
        None,
    );
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
    let verdicts = select(
        entries,
        DAY * 7,
        Some(100),
        now,
        &mut |_, freed| (Fate::Paused, freed),
        None,
    );
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
    let verdicts = select(
        vec![unreadable],
        DAY * 7,
        Some(0),
        now,
        &mut |_, _| panic!("an unreadable entry must not be claimed"),
        None,
    );
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
        started: |_| None,
        proofs: RefCell::default(),
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

/// An identity whose processes all started at `started`.
fn identity_started(home: &Path, started: fn(i32) -> Option<SystemTime>) -> Identity {
    Identity {
        started,
        ..Identity::current(home)
    }
}

/// Writes an executable named `agent-ide` below `dir` containing each of `proofs`.
fn fake_build_with(dir: &Path, proofs: &[&[u8]]) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let exe = dir.join("agent-ide");
    let mut bytes = vec![0xCFu8; 3 << 20];
    for (index, proof) in proofs.iter().enumerate() {
        // The first straddles the first read boundary of the scanner, which reads
        // `(1 << 20) + proof length` bytes at a time, as a proof in the middle of a real binary
        // may; the rest sit a megabyte further on each.
        let at = (1 << 20) + proof.len() - 7 + index * (1 << 20);
        bytes[at..at + proof.len()].copy_from_slice(proof);
    }
    fs::write(&exe, bytes).unwrap();
    exe
}

/// Writes an executable named `agent-ide` below `dir`; `proven` embeds both build proofs, as
/// every build of this source does.
fn fake_build(dir: &Path, proven: bool) -> PathBuf {
    if proven {
        fake_build_with(dir, &[LEASE_BUILD_PROOF, HINT_LOCK_BUILD_PROOF])
    } else {
        fake_build_with(dir, &[])
    }
}

/// A development or test build proven to take leases does not pause eviction, an installed
/// release up to the boundary still does, and the report names both groups.
#[test]
fn a_proven_test_build_does_not_pause_eviction_but_an_old_release_does() {
    let home = scratch("test-build");
    let dir = check_cache(&home, &home.join("deleted"));
    let build = fake_build(&home.join("gate/target/debug"), true);
    let identity = identity_started(&home, |_| Some(SystemTime::now() + DAY));
    let with_build = || Some(vec![(41, Some(build.clone()))]);

    let report = sweep_as(&identity, &home, false, SystemTime::now(), &with_build);
    assert!(!report.paused(), "{report:?}");
    assert_eq!(verdict(&report, &dir).fate, Fate::Removed);
    let text = report.render(false);
    assert!(
        text.contains("take leases and do not pause eviction"),
        "{text}"
    );
    assert!(text.contains("pid 41"), "{text}");

    let old = home.join("standalone/releases/0.9.1/agent-ide");
    let both = || Some(vec![(41, Some(build.clone())), (42, Some(old.clone()))]);
    let report = sweep_as(&identity, &home, true, SystemTime::now(), &both);
    assert!(report.paused());
    assert_eq!(verdict(&report, &dir).fate, Fate::Paused);
    assert!(dir.exists());
    let text = report.render(true);
    assert!(text.contains("pid 42") && text.contains("pid 41"), "{text}");
}

/// Only a proof inside a file that is no newer than the process proves a build: a lookalike
/// without the proof, a rebuild after the process started, and an unreadable path stay legacy.
#[test]
fn an_unproven_or_rebuilt_dev_executable_stays_legacy() {
    let home = scratch("unproven-build");
    let unproven = fake_build(&home.join("a/target/debug"), false);
    let proven = fake_build(&home.join("b/target/debug"), true);
    let processes = vec![
        (1, Some(unproven.clone())),
        (2, Some(proven.clone())),
        (3, Some(home.join("gone/target/debug/agent-ide"))),
    ];
    let later = identity_started(&home, |_| Some(SystemTime::now() + DAY));
    let pids = |classified: Vec<Process>| classified.into_iter().map(|p| p.0).collect::<Vec<_>>();
    let classified = later.classify(&processes);
    assert_eq!(pids(classified.legacy), [1, 3]);
    assert_eq!(pids(classified.builds), [2]);

    // The same file, but the process started before it was written: it runs older code.
    let earlier = identity_started(&home, |_| Some(SystemTime::UNIX_EPOCH));
    assert_eq!(pids(earlier.classify(&processes).legacy), [1, 2, 3]);
    // A process whose start cannot be read is unproven.
    let unknown = identity_started(&home, |_| None);
    assert_eq!(pids(unknown.classify(&processes).legacy), [1, 2, 3]);
}

/// The real start time of this process is readable and not in the future.
#[test]
fn process_start_reads_the_real_start_time() {
    let started = process_start(std::process::id() as i32).expect("own start time");
    assert!(started <= SystemTime::now());
    assert!(started > SystemTime::now() - 30 * DAY);
}

/// The participation floor is the legacy boundary whatever this build's version: every release
/// after it takes leases, so an older lease-taking release still in use (a session started before
/// an upgrade) never pauses the newer one's sweep.
#[test]
fn every_release_after_the_legacy_boundary_participates() {
    let home = scratch("floor");
    let identity = Identity::current(&home);
    assert_eq!(identity.floor, LEGACY_BOUNDARY);
    let first_leasing = identity.releases.join("0.10.0/agent-ide");
    assert!(identity.participates(&first_leasing));
    assert!(!identity.participates(&identity.releases.join("0.9.1/agent-ide")));
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
    let (fate, _) = claim_entry(&entries[0], 0, &locks, SystemTime::now(), true, &|| true);
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

/// Creates `<home>/telemetry/<digest of launch>` as a private store with a small database file.
fn marker_less_store(home: &Path, launch: &Path) -> PathBuf {
    let store = home.join("telemetry").join(
        blake3::hash(launch.as_os_str().as_bytes())
            .to_hex()
            .as_str(),
    );
    fs::create_dir_all(&store).unwrap();
    for dir in [home.join("telemetry"), store.clone()] {
        fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    }
    fs::write(store.join("state.sqlite"), vec![1u8; 4096]).unwrap();
    store
}

/// Adopting publishes a private marker naming the launch directory, leaves a correct one alone,
/// migrates a historical `0644` one and repairs a torn one, never leaving a temporary file.
#[test]
fn adopting_a_marker_publishes_migrates_and_repairs_atomically() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = scratch("adopt");
    let launch = home.join("project");
    let store = marker_less_store(&home, &launch);
    let marker = store.join(MARKER_FILE_NAME);
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let id = |path: &Path| fs::metadata(path).unwrap().ino();

    adopt_marker(&home, &store, &launch).unwrap();
    assert_eq!(fs::read(&marker).unwrap(), launch.as_os_str().as_bytes());
    assert_eq!(mode(&marker), 0o600);

    // A correct private marker is not rewritten.
    let before = id(&marker);
    adopt_marker(&home, &store, &launch).unwrap();
    assert_eq!(id(&marker), before);

    // A 0.10.6 marker (`fs::write` under umask 022) is safe and migrated to 0600.
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
    adopt_marker(&home, &store, &launch).unwrap();
    assert_eq!(mode(&marker), 0o600);
    assert_ne!(id(&marker), before);

    // A torn marker is replaced whole, and one writable by others is refused.
    fs::write(&marker, b"/proj").unwrap();
    adopt_marker(&home, &store, &launch).unwrap();
    assert_eq!(fs::read(&marker).unwrap(), launch.as_os_str().as_bytes());
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(adopt_marker(&home, &store, &launch).is_err());
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();

    let names: Vec<_> = fs::read_dir(&store)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        names.len(),
        2,
        "only the database and the marker remain: {names:?}"
    );
}

/// A symlink or directory at the marker path, a store outside `<root>/telemetry`, one that is not
/// named by the launch's digest, and a nonprivate store are all refused without touching anything.
#[test]
fn adopting_a_marker_refuses_unsafe_or_misplaced_state() {
    let home = scratch("adopt-refused");
    let launch = home.join("project");
    let store = marker_less_store(&home, &launch);
    let marker = store.join(MARKER_FILE_NAME);
    let outside = home.join("outside");
    fs::write(&outside, b"keep").unwrap();

    std::os::unix::fs::symlink(&outside, &marker).unwrap();
    assert!(adopt_marker(&home, &store, &launch).is_err());
    assert_eq!(fs::read(&outside).unwrap(), b"keep");
    fs::remove_file(&marker).unwrap();
    fs::create_dir(&marker).unwrap();
    assert!(adopt_marker(&home, &store, &launch).is_err());
    fs::remove_dir(&marker).unwrap();

    // Wrong root, wrong name, nonprivate store, replaced-by-symlink store.
    let other_root = scratch("adopt-other-root");
    assert!(adopt_marker(&other_root, &store, &launch).is_err());
    assert!(adopt_marker(&home, &store, &home.join("another")).is_err());
    let loose = marker_less_store(&home, &home.join("loose"));
    fs::set_permissions(&loose, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    assert!(adopt_marker(&home, &loose, &home.join("loose")).is_err());
    let moved = home.join("moved-away");
    fs::rename(&store, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &store).unwrap();
    assert!(adopt_marker(&home, &store, &launch).is_err());
    assert!(!moved.join(MARKER_FILE_NAME).exists());
}

/// A live telemetry writer (its lifetime lock held) does not stop the marker from being
/// published, and the lock stays held.
#[test]
fn adopting_a_marker_leaves_a_live_writer_alone() {
    let home = scratch("adopt-writer");
    let launch = home.join("project");
    let store = marker_less_store(&home, &launch);
    let writer = try_exclusive(&store.join(TELEMETRY_LOCK)).expect("writer lock");
    adopt_marker(&home, &store, &launch).unwrap();
    assert_eq!(
        fs::read(store.join(MARKER_FILE_NAME)).unwrap(),
        launch.as_os_str().as_bytes()
    );
    assert!(
        try_exclusive(&store.join(TELEMETRY_LOCK)).is_none(),
        "the writer still holds its lock"
    );
    drop(writer);
}

/// Adoption waits for a sweeper's exclusive claim, and refuses when the store it validated was
/// moved to trash and replaced meanwhile instead of publishing into the stranger.
#[test]
fn adopting_a_marker_waits_for_a_claim_and_revalidates_the_store() {
    let home = scratch("adopt-claim");
    let launch = home.join("project");
    let store = marker_less_store(&home, &launch);
    let key = worktree_key(&launch);
    let claim = try_exclusive(&home.join(LOCKS_DIR).join(format!("{key}.lock"))).expect("claim");

    let adopting = {
        let (home, store, launch) = (home.to_path_buf(), store.clone(), launch.clone());
        std::thread::spawn(move || adopt_marker(&home, &store, &launch))
    };
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !adopting.is_finished(),
        "adoption waits behind the exclusive claim"
    );
    // The sweeper's rename: the validated directory is gone, another one takes its name.
    fs::rename(&store, home.join("trashed")).unwrap();
    let replacement = marker_less_store(&home, &launch);
    drop(claim);
    assert!(
        adopting.join().unwrap().is_err(),
        "the replaced store is refused"
    );
    assert!(!replacement.join(MARKER_FILE_NAME).exists());
}

/// The report's starvation case: a marker-less store is held by any unrelated lease, the adopted
/// store (its launch directory gone) is claimable at once.
#[test]
fn an_adopted_store_is_no_longer_protected_by_an_unrelated_lease() {
    let home = scratch("adopt-starvation");
    let launch = home.join("deleted-project");
    let store = marker_less_store(&home, &launch);
    backdate(&store, 40 * DAY);
    let unrelated = Lease::acquire(&home, &home.join("somewhere-else")).expect("lease");
    let kept = sweep_with(&home, false, SystemTime::now(), &nobody);
    assert_eq!(
        verdict(&kept, &store).fate,
        Fate::InUse,
        "marker-less waits for any lease"
    );

    // Adoption takes the launch lease as well as the unrelated one held here.
    adopt_marker(&home, &store, &launch).unwrap();
    backdate(&store, 40 * DAY);
    let freed = sweep_with(&home, false, SystemTime::now(), &nobody);
    assert_eq!(verdict(&freed, &store).fate, Fate::Removed);
    drop(unrelated);
}

/// Hook key hints are collected only while every live `agent-ide` process is this executable or a
/// proven build: an installed release without the proof (whatever its version) may refresh a hint
/// without the directory lock, as may an unreadable or unverifiable executable.
#[test]
fn hint_collection_pauses_for_any_publisher_not_proven_to_lock() {
    let home = scratch("hint-publishers");
    let proven = fake_build(&home.join("gate/target/debug"), true);
    let unproven = fake_build(&home.join("old/releases/0.10.6"), false);
    let later = identity_started(&home, |_| Some(SystemTime::now() + DAY));
    let pids = |list: Option<Vec<Process>>| {
        list.map(|list| list.into_iter().map(|(pid, _)| pid).collect::<Vec<_>>())
    };
    let snapshot = |processes: Vec<Process>| move || Some(processes.clone());

    let calm = snapshot(vec![(1, Some(proven.clone()))]);
    assert_eq!(pids(hint_publishers_unsafe_as(&later, &calm)), Some(vec![]));
    let own = std::env::current_exe().unwrap();
    let with_self = snapshot(vec![(1, Some(proven.clone())), (2, Some(own))]);
    assert_eq!(
        pids(hint_publishers_unsafe_as(&later, &with_self)),
        Some(vec![])
    );

    let mixed = snapshot(vec![
        (1, Some(proven.clone())),
        (2, Some(unproven)),
        (3, None),
        (4, Some(home.join("gone/agent-ide"))),
    ]);
    assert_eq!(
        pids(hint_publishers_unsafe_as(&later, &mixed)),
        Some(vec![2, 3, 4])
    );

    // A rebuilt-after-start file or an unreadable start time proves nothing either.
    let earlier = identity_started(&home, |_| Some(SystemTime::UNIX_EPOCH));
    assert_eq!(
        pids(hint_publishers_unsafe_as(&earlier, &calm)),
        Some(vec![1])
    );
    assert_eq!(pids(hint_publishers_unsafe_as(&later, &|| None)), None);
}

/// A build that has the lease proof but predates the hint lock (a development build between the
/// two changes) takes leases, yet may publish hints without the directory lock: it is a build for
/// eviction and an unsafe publisher for hint collection, whichever is asked first.
#[test]
fn a_lease_only_build_is_not_a_hint_locking_publisher() {
    let home = scratch("lease-only");
    let lease_only = fake_build_with(&home.join("mid/target/debug"), &[LEASE_BUILD_PROOF]);
    let hint_only = fake_build_with(&home.join("odd/target/debug"), &[HINT_LOCK_BUILD_PROOF]);
    let later = identity_started(&home, |_| Some(SystemTime::now() + DAY));
    let processes = vec![(1, Some(lease_only)), (2, Some(hint_only))];
    let pids = |list: Vec<Process>| list.into_iter().map(|(pid, _)| pid).collect::<Vec<_>>();

    // Either question first: the cache must keep the two proofs apart.
    assert_eq!(pids(later.hint_unsafe(&processes)), [1]);
    assert_eq!(pids(later.classify(&processes).builds), [1]);
    let again = identity_started(&home, |_| Some(SystemTime::now() + DAY));
    assert_eq!(pids(again.classify(&processes).builds), [1]);
    assert_eq!(pids(again.hint_unsafe(&processes)), [1]);
}

/// Writes `len` bytes of non-sparse data to `path`, creating parents.
fn big_file(path: &Path, len: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![0x5au8; len]).unwrap();
}

/// A check cache named by `name` holding one `unique` file of its own.
fn sharing_cache(home: &Path, name: &str, unique: usize) -> PathBuf {
    let worktree = home.join(format!("wt-{name}"));
    fs::create_dir_all(&worktree).unwrap();
    let dir = home.join("checks").join(REPO).join(worktree_key(&worktree));
    big_file(&dir.join("digest/lang/target/unique"), unique);
    fs::write(dir.join(MARKER_FILE_NAME), worktree.as_os_str().as_bytes()).unwrap();
    backdate(&dir, DAY);
    dir
}

/// Entries of the fixture family, oldest first.
fn scanned(home: &Path) -> Vec<Entry> {
    let (mut entries, complete) = scan_checks(&home.join("checks"), &home.join(LOCKS_DIR));
    assert!(complete);
    entries.sort_by_key(|entry| entry.last_used);
    entries
}

/// Three caches, two of which hold hard links to one large file. The budget counts the file once,
/// removing the first sharer frees only what no survivor holds, and the last sharer then frees the
/// file with it.
#[test]
fn hard_links_are_charged_once_and_a_removal_frees_only_what_no_survivor_holds() {
    let home = scratch("hard-links");
    let first = sharing_cache(&home, "a", 64 << 10);
    let second = sharing_cache(&home, "b", 64 << 10);
    let third = sharing_cache(&home, "c", 64 << 10);
    big_file(&first.join("digest/lang/target/shared"), 256 << 10);
    fs::hard_link(
        first.join("digest/lang/target/shared"),
        second.join("digest/lang/target/shared"),
    )
    .unwrap();
    // The shared inode takes the last mtime set: the older cache goes last.
    backdate(&second, DAY * 2);
    backdate(&first, DAY * 3);
    let entries = scanned(&home);
    let shared = fs::metadata(first.join("digest/lang/target/shared"))
        .unwrap()
        .blocks()
        * 512;
    let alone: u64 = entries.iter().map(|entry| entry.bytes).sum();

    let sharing = Sharing::new(entries.iter().map(|entry| &entry.usage));
    assert_eq!(
        sharing.charged(),
        alone - shared,
        "the link is charged once"
    );
    assert!(sharing.logical() >= (3 * 64 + 2 * 256) << 10);
    let now = SystemTime::now();
    let mut freed_by = Vec::new();
    // Exactly the deduplicated total fits: a per-entry sum would have evicted the first sharer.
    let fits = select(
        entries.clone(),
        DAY * 30,
        Some(sharing.charged()),
        now,
        &mut |_, freed| {
            freed_by.push(freed);
            (Fate::Removed, freed)
        },
        None,
    );
    assert!(fits.is_empty(), "{fits:?}");

    // One byte over: the oldest sharer goes and frees its own file but not the link.
    let verdicts = select(
        entries.clone(),
        DAY * 30,
        Some(sharing.charged() - 1),
        now,
        &mut |_, freed| (Fate::Removed, freed),
        None,
    );
    assert_eq!(verdicts.len(), 1, "{verdicts:?}");
    assert!(verdicts[0].path.starts_with(&first));
    assert_eq!(verdicts[0].bytes, entries[0].bytes - shared);

    // Down to nothing: the second sharer now frees the file as well.
    let verdicts = select(
        entries.clone(),
        DAY * 30,
        Some(0),
        now,
        &mut |_, freed| (Fate::Removed, freed),
        None,
    );
    let freed: Vec<_> = verdicts.iter().map(|verdict| verdict.bytes).collect();
    assert_eq!(
        freed,
        [
            entries[0].bytes - shared,
            entries[1].bytes,
            entries[2].bytes
        ]
    );
    assert!(verdicts[1].path.starts_with(&second));
    drop(third);
}

/// A perfect APFS clone is charged once; private bytes are what the volume gets back and never
/// exceed the charge.
#[cfg(target_os = "macos")]
#[test]
fn perfect_clones_are_charged_once_and_private_bytes_are_an_estimate() {
    let home = scratch("clones");
    let origin = sharing_cache(&home, "origin", 32 << 10);
    let sibling = sharing_cache(&home, "sibling", 32 << 10);
    big_file(&origin.join("digest/lang/target/dep"), 1 << 20);
    let clone = std::process::Command::new("/bin/cp")
        .arg("-c")
        .arg(origin.join("digest/lang/target/dep"))
        .arg(sibling.join("digest/lang/target/dep"))
        .status()
        .unwrap();
    assert!(clone.success());
    let entries = scanned(&home);
    let alone: u64 = entries.iter().map(|entry| entry.bytes).sum();
    let sharing = Sharing::new(entries.iter().map(|entry| &entry.usage));
    let dep = fs::metadata(origin.join("digest/lang/target/dep"))
        .unwrap()
        .blocks()
        * 512;

    assert_eq!(
        sharing.charged(),
        alone - dep,
        "the clone stream is charged once"
    );
    assert!(
        sharing.private() < sharing.charged(),
        "shared extents are not private"
    );
    assert!(sharing.logical() >= (2 << 20));

    let report = sweep_with(&home, false, SystemTime::now(), &nobody);
    assert_eq!(report.checks.bytes, sharing.charged());
    assert_eq!(report.checks.logical, sharing.logical());
    assert_eq!(report.checks.private, sharing.private());
    let text = report.render(false);
    assert!(text.contains("charged ("), "{text}");
    assert!(
        text.contains("logical") && text.contains("private reclaim estimate"),
        "{text}"
    );
}

/// One rustc crate directory: finalized sessions named `finalized` (oldest to newest naming is the
/// caller's), plus a working session, each with its lock file.
fn rustc_cache(home: &Path, name: &str, finalized: &[&str]) -> PathBuf {
    let dir = sharing_cache(home, name, 8 << 10);
    let target = dir.join("digest/lang/target/debug");
    big_file(&target.join("deps/libdep.rlib"), 16 << 10);
    let krate = target.join("incremental/krate-1abc");
    for session in finalized.iter().chain(&["s-zz-w1-working"]) {
        big_file(&krate.join(session).join("work-products.bin"), 16 << 10);
        let stem = session.rsplit_once('-').unwrap().0;
        fs::write(krate.join(format!("{stem}.lock")), b"").unwrap();
    }
    dir
}

/// Names of the session directories left in the fixture crate directory.
fn sessions_left(cache: &Path) -> Vec<String> {
    let mut names: Vec<_> =
        fs::read_dir(cache.join("digest/lang/target/debug/incremental/krate-1abc"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| !name.ends_with(".lock"))
            .collect();
    names.sort();
    names
}

/// A recent worktree keeps the newest finalized session per crate by decoded timestamp (not text
/// order, not mtime) and its `-working` session; older finalized sessions and their locks go.
#[test]
fn older_finalized_sessions_are_swept_keeping_the_newest_and_working() {
    let home = scratch("sessions");
    // Numerically: z (35) < 10 (36) < 1a (46); text order would keep `z`.
    let cache = rustc_cache(&home, "s", &["s-z-a1-h1", "s-10-b2-h2", "s-1a-c3-h3"]);
    // The newest session has the oldest mtime: mtime must not decide.
    backdate(
        &cache.join("digest/lang/target/debug/incremental/krate-1abc/s-1a-c3-h3"),
        DAY * 5,
    );

    let report = sweep_with(&home, true, SystemTime::now(), &nobody);

    let swept = verdict(&report, &cache);
    assert_eq!(
        (swept.reason, swept.fate),
        (Reason::Sessions, Fate::Removed)
    );
    assert!(swept.bytes >= 32 << 10, "{swept:?}");
    assert_eq!(sessions_left(&cache), ["s-1a-c3-h3", "s-zz-w1-working"]);
    let krate = cache.join("digest/lang/target/debug/incremental/krate-1abc");
    assert!(!krate.join("s-z-a1.lock").exists() && !krate.join("s-10-b2.lock").exists());
    assert!(krate.join("s-1a-c3.lock").exists() && krate.join("s-zz-w1.lock").exists());
    assert!(
        cache
            .join("digest/lang/target/debug/deps/libdep.rlib")
            .exists()
    );
}

/// rustc's own session lock protects a session: one held (by a compiler reading or collecting it)
/// is kept, and it goes once the lock is released.
#[test]
fn a_session_whose_rustc_lock_is_held_is_kept() {
    let home = scratch("session-lock");
    let cache = rustc_cache(&home, "l", &["s-a-a1-h1", "s-b-b2-h2", "s-c-c3-h3"]);
    let krate = cache.join("digest/lang/target/debug/incremental/krate-1abc");
    let held = File::open(krate.join("s-a-a1.lock")).unwrap();
    assert!(flock(&held, libc::LOCK_SH | libc::LOCK_NB));

    sweep_with(&home, true, SystemTime::now(), &nobody);
    assert_eq!(
        sessions_left(&cache),
        ["s-a-a1-h1", "s-c-c3-h3", "s-zz-w1-working"]
    );

    drop(held);
    for _ in 0..300 {
        sweep_with(&home, true, SystemTime::now(), &nobody);
        if sessions_left(&cache).len() == 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(sessions_left(&cache), ["s-c-c3-h3", "s-zz-w1-working"]);
}

/// Anything not provably rustc's is left alone: a malformed `s-` entry makes the whole crate
/// uncertain, a tie at the newest timestamp keeps both, a lone session is never removed.
#[test]
fn uncertain_crates_and_ties_are_left_alone() {
    let home = scratch("session-uncertain");
    let malformed = rustc_cache(&home, "m", &["s-a-a1-h1", "s-b-b2-h2"]);
    big_file(
        &malformed.join("digest/lang/target/debug/incremental/krate-1abc/s-Not_A_Session/x"),
        10,
    );
    let tied = rustc_cache(&home, "t", &["s-a-a1-h1", "s-b-b2-h2", "s-b-c3-h3"]);
    let lone = rustc_cache(&home, "o", &["s-a-a1-h1"]);

    let report = sweep_with(&home, true, SystemTime::now(), &nobody);

    assert!(report.verdicts.is_empty(), "{report:?}");
    assert_eq!(sessions_left(&malformed).len(), 4);
    assert_eq!(sessions_left(&tied).len(), 4);
    assert_eq!(sessions_left(&lone).len(), 2);
}

/// A dry run reports the sweep without removing anything or creating a lock file.
#[test]
fn a_dry_run_reports_session_sweeps_without_touching_them() {
    let home = scratch("session-dry");
    let cache = rustc_cache(&home, "d", &["s-a-a1-h1", "s-b-b2-h2"]);
    let krate = cache.join("digest/lang/target/debug/incremental/krate-1abc");
    fs::remove_file(krate.join("s-a-a1.lock")).unwrap();

    let report = sweep_with(&home, false, SystemTime::now(), &nobody);

    assert!(
        report
            .render(false)
            .contains("would remove: checks sessions")
    );
    assert_eq!(sessions_left(&cache).len(), 3);
    assert!(!krate.join("s-a-a1.lock").exists());
}

/// A leased worktree keeps every session.
#[test]
fn a_leased_worktree_keeps_its_sessions() {
    let home = scratch("session-leased");
    let cache = rustc_cache(&home, "x", &["s-a-a1-h1", "s-b-b2-h2"]);
    let worktree = home.join("wt-x");
    let lease = Lease::acquire(&home, &worktree).expect("lease");

    let report = sweep_with(&home, true, SystemTime::now(), &nobody);

    assert_eq!(verdict(&report, &cache).fate, Fate::InUse);
    assert_eq!(sessions_left(&cache).len(), 3);
    drop(lease);
}

/// A worktree idle past [`INCREMENTAL_IDLE`] loses its whole `incremental` tier but keeps its
/// dependencies and stays; one idle past the idle age goes whole, without a tier verdict first.
#[test]
fn an_idle_worktree_loses_incremental_state_before_the_worktree() {
    let home = scratch("incremental-tier");
    let idle = rustc_cache(&home, "i", &["s-a-a1-h1", "s-b-b2-h2"]);
    let expired = rustc_cache(&home, "e", &["s-a-a1-h1", "s-b-b2-h2"]);
    backdate(&idle, INCREMENTAL_IDLE + DAY);
    backdate(&expired, CHECKS_IDLE + DAY);

    let report = sweep_with(&home, true, SystemTime::now(), &nobody);

    let tier = verdict(&report, &idle);
    assert_eq!(
        (tier.reason, tier.fate),
        (Reason::Incremental, Fate::Removed)
    );
    assert!(!idle.join("digest/lang/target/debug/incremental").exists());
    assert!(
        idle.join("digest/lang/target/debug/deps/libdep.rlib")
            .exists()
    );
    assert!(
        fs::read_dir(home.join("checks/.trash"))
            .unwrap()
            .next()
            .is_none()
    );
    let whole = verdict(&report, &expired);
    assert_eq!((whole.reason, whole.fate), (Reason::Idle, Fate::Removed));
    assert!(!expired.exists());
    assert_eq!(report.verdicts.len(), 2, "{report:?}");
}

/// Over the budget, every idle worktree's incremental tier is trimmed before any whole
/// worktree is evicted.
#[test]
fn the_incremental_tier_goes_before_any_whole_worktree_is_evicted() {
    let now = SystemTime::UNIX_EPOCH + DAY * 100;
    let entries = vec![
        synthetic("old", 100, 5),
        synthetic("older", 100, 6),
        synthetic("fresh", 100, 1),
    ];
    let mut order = Vec::new();
    let mut trim = |entry: &Entry, tier: Tier| {
        order.push(format!(
            "trim {tier:?} {}",
            entry.path.file_name().unwrap().to_string_lossy()
        ));
        (tier == Tier::Incremental).then(|| Trimmed {
            fate: Fate::Removed,
            usage: Some(Usage {
                own: 40,
                ..Usage::default()
            }),
        })
    };
    let verdicts = select(
        entries,
        DAY * 30,
        Some(150),
        now,
        &mut |_, freed| (Fate::Removed, freed),
        Some(&mut trim),
    );
    let reasons: Vec<_> = verdicts
        .iter()
        .map(|verdict| {
            (
                verdict
                    .path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
                verdict.reason,
            )
        })
        .collect();
    // 300 → trimming the two idle ones leaves 40+40+100 = 180 → still over: the LRU one goes.
    assert_eq!(
        reasons,
        [
            ("older".to_owned(), Reason::Incremental),
            ("old".to_owned(), Reason::Incremental),
            ("older".to_owned(), Reason::Budget),
        ]
    );
}

/// A `target` that is a symlink to a caller-owned directory is never searched: neither tier
/// reaches the `incremental` state behind it, and a symlinked `incremental` or session is left too.
#[test]
fn symlinked_target_incremental_and_session_directories_are_never_followed() {
    let home = scratch("session-symlinks");
    let cache = rustc_cache(&home, "y", &["s-a-a1-h1", "s-b-b2-h2"]);
    let callers = home.join("callers-target");
    fs::rename(cache.join("digest/lang/target"), &callers).unwrap();
    std::os::unix::fs::symlink(&callers, cache.join("digest/lang/target")).unwrap();
    backdate(&cache, INCREMENTAL_IDLE + DAY);
    let incremental = callers.join("debug/incremental");

    let recent = sweep_with(&home, true, SystemTime::now(), &nobody);
    assert!(recent.verdicts.is_empty(), "{recent:?}");
    assert!(incremental.join("krate-1abc/s-a-a1-h1").exists());

    // The same holds for a symlinked `incremental` directory and symlinked session directories.
    fs::remove_file(cache.join("digest/lang/target")).unwrap();
    let target = cache.join("digest/lang/target");
    fs::create_dir_all(target.join("debug")).unwrap();
    std::os::unix::fs::symlink(&incremental, target.join("debug/incremental")).unwrap();
    let report = sweep_with(&home, true, SystemTime::now(), &nobody);
    assert!(report.verdicts.is_empty(), "{report:?}");
    assert!(incremental.join("krate-1abc/s-a-a1-h1").exists());
    assert!(incremental.join("krate-1abc/s-b-b2-h2").exists());
}

/// A trim reports what the family charge actually drops by: a file of the removed `incremental`
/// tier that a recent worktree still holds through a hard link stays charged and is not freed.
#[test]
fn a_trim_does_not_report_bytes_a_surviving_entry_still_holds_as_freed() {
    let home = scratch("trim-shared");
    let idle = rustc_cache(&home, "i", &["s-a-a1-h1", "s-b-b2-h2"]);
    let recent = sharing_cache(&home, "r", 8 << 10);
    let held =
        idle.join("digest/lang/target/debug/incremental/krate-1abc/s-b-b2-h2/work-products.bin");
    fs::hard_link(&held, recent.join("digest/lang/target/linked")).unwrap();
    // The shared inode keeps the last mtime set: the idle cache goes last.
    backdate(&recent, Duration::from_secs(60));
    backdate(&idle, INCREMENTAL_IDLE + DAY);
    let survivor = fs::metadata(&held).unwrap().blocks() * 512;
    let tier_alone = measure_usage(&idle.join("digest/lang/target/debug/incremental"))
        .0
        .standalone();
    let before = Sharing::new(scanned(&home).iter().map(|entry| &entry.usage)).charged();

    let report = sweep_with(&home, true, SystemTime::now(), &nobody);

    let trimmed = verdict(&report, &idle);
    assert_eq!(
        (trimmed.reason, trimmed.fate),
        (Reason::Incremental, Fate::Removed)
    );
    let after = Sharing::new(scanned(&home).iter().map(|entry| &entry.usage)).charged();
    assert_eq!(
        trimmed.bytes,
        before - after,
        "freed is the drop of the family charge"
    );
    assert_eq!(trimmed.bytes + survivor, tier_alone, "{trimmed:?}");
}

/// Private bytes count an inode only when every one of its links is inside the family: a link
/// outside keeps the data allocated, so removing the cache reclaims nothing of it. The charge
/// is unchanged.
#[cfg(target_os = "macos")]
#[test]
fn the_private_estimate_excludes_inodes_with_a_link_outside_the_family() {
    let home = scratch("private-outside-link");
    let cache = sharing_cache(&home, "p", 4096);
    big_file(&cache.join("digest/lang/target/pinned"), 256 << 10);
    big_file(&cache.join("digest/lang/target/free"), 256 << 10);
    let outside = scratch("private-outside-elsewhere").join("keeper");
    fs::hard_link(cache.join("digest/lang/target/pinned"), &outside).unwrap();
    let entries = scanned(&home);
    let sharing = Sharing::new(entries.iter().map(|entry| &entry.usage));
    let pinned = fs::metadata(&outside).unwrap().blocks() * 512;
    assert!(sharing.charged() >= pinned * 2);
    assert!(
        sharing.private() <= sharing.charged() - pinned,
        "{} private vs {} charged",
        sharing.private(),
        sharing.charged()
    );
    assert!(
        sharing.private() >= pinned,
        "the unpinned file is reclaimable"
    );
}

/// The session lock the sweep takes excludes a lock held the way a compiler process takes it: a
/// POSIX `fcntl` read lock of another process on the same file.
#[cfg(target_os = "macos")]
#[test]
fn a_posix_lock_held_by_another_process_keeps_the_session() {
    use std::io::{BufRead, Write};
    let home = scratch("session-fcntl");
    let lock = home.join("s-a-a1.lock");
    fs::write(&lock, b"").unwrap();
    // struct flock on Darwin: off_t start, off_t len, pid_t pid, short type, short whence.
    let mut holder = std::process::Command::new("/usr/bin/perl")
        .args([
            "-MFcntl",
            "-e",
            r#"open(F, "+<", $ARGV[0]) or die; my $lock = pack("q q i s s", 0, 0, 0, F_RDLCK, 0); fcntl(F, F_SETLK, $lock) or die "lock: $!"; $| = 1; print "locked\n"; <STDIN>;"#,
        ])
        .arg(&lock)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(holder.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "locked\n");

    assert!(
        lock_session(&lock, true).is_none(),
        "a compiler's lock is honoured"
    );

    holder.stdin.take().unwrap().write_all(b"\n").unwrap();
    holder.wait().unwrap();
    assert!(lock_session(&lock, true).is_some_and(|file| file.is_some()));
}
