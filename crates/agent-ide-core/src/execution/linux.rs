//! Linux confinement for background checks: private namespaces and an inherited socket filter.
//!
//! Bubblewrap constructs an empty filesystem, mounts declared roots with their exact access,
//! and installs the filter before executing any project code. Unsupported policies or host
//! facilities fail closed; this module never retries a command outside confinement.

use std::{
    ffi::{CString, OsString},
    fs::File,
    io::{self, Seek, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use super::seatbelt::SeatbeltPolicy;

/// The system-installed launcher, measured again by Execution on admitted launches.
pub(super) const BWRAP: &str = "/usr/bin/bwrap";

/// Prepared argv and the sealed filter which bubblewrap consumes through its stdin.
pub(super) struct Launch {
    pub args: Vec<OsString>,
    pub filter: File,
    /// Pinned mount sources survive launch; the child inherits only these descriptors.
    pub mounts: Vec<Arc<File>>,
}

/// Builds a policy-preserving Linux launch, refusing exclusions we cannot enforce dynamically.
///
/// The fixed shell only restores null stdin after bubblewrap consumes the filter on fd 0.
/// All project arguments remain separate argv entries; no project bytes become shell syntax.
/// Absent read-only grants mount nothing, even if those paths appear later; writable roots
/// must already exist. Errors other than absence and races opening resolved roots fail closed.
pub(super) fn prepare(
    program: &Path,
    args: &[OsString],
    cwd: &Path,
    policy: &SeatbeltPolicy,
) -> io::Result<Launch> {
    if !policy.read_denies.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Linux check confinement cannot enforce read exclusions",
        ));
    }
    if !normal_absolute(program) || !normal_absolute(cwd) {
        return Err(invalid_policy());
    }
    let mut mounts = Vec::new();
    // These are runtime files, not a bind of /usr, /etc, /home or the host root.
    for path in [
        "/usr/bin",
        "/usr/lib",
        "/usr/lib64",
        // GCC runs cc1 and collect2 from this system compiler-specific directory.
        "/usr/libexec/gcc",
        "/usr/include",
        "/bin",
        "/lib",
        "/lib64",
        "/etc/ld.so.cache",
        "/etc/localtime",
    ] {
        let canonical = match std::fs::canonicalize(path) {
            Ok(canonical) => canonical,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        mounts.push((
            PathBuf::from(path),
            false,
            Arc::new(open_mount(&canonical)?),
        ));
    }
    for (roots, writable) in [(&policy.read_roots, false), (&policy.write_roots, true)] {
        for root in roots {
            if !normal_absolute(root) || reserved_root(root) {
                return Err(invalid_policy());
            }
            let canonical = match std::fs::canonicalize(root) {
                Ok(canonical) => canonical,
                Err(error) if !writable && error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            // A grant over a synthetic system tree would replace our isolation primitives.
            // Refuse both canonical and alias spellings, rather than silently narrow the grant.
            if reserved_root(&canonical) {
                return Err(invalid_policy());
            }
            let source = Arc::new(open_mount(&canonical)?);
            mounts.push((canonical.clone(), writable, source.clone()));
            if root != &canonical {
                mounts.push((root.clone(), writable, source));
            }
        }
    }
    // Parents precede children, so a private writable child can override a read-only parent.
    // Equal roots retain the additive policy's writable grant.
    mounts.sort_by(|(left, lw, _), (right, rw, _)| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then(left.cmp(right))
            .then(lw.cmp(rw))
    });
    mounts.dedup_by(|(left, lw, _), (right, rw, _)| left == right && lw == rw);
    let mut argv: Vec<OsString> = [
        "--unshare-user",
        "--unshare-pid",
        "--unshare-net",
        "--unshare-ipc",
        "--unshare-uts",
        "--disable-userns",
        "--assert-userns-disabled",
        "--die-with-parent",
        "--new-session",
        "--cap-drop",
        "ALL",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    let mut pinned = Vec::new();
    for (path, writable, source) in mounts {
        // Bubblewrap consumes each bind-fd once; aliases need distinct owned descriptors.
        let source = Arc::new(source.try_clone()?);
        argv.push(OsString::from(if writable {
            "--bind-fd"
        } else {
            "--ro-bind-fd"
        }));
        argv.push(OsString::from(source.as_raw_fd().to_string()));
        argv.push(path.into_os_string());
        pinned.push(source);
    }
    argv.extend(["--proc", "/proc", "--dir", "/dev"].map(OsString::from));
    // These standard null/zero/random devices expose no host file contents or persistent
    // filesystem writes. Do not apply remount-ro: bubblewrap also sets nodev on that operation,
    // which makes even read access to these explicitly allowed devices fail with EACCES.
    for device in ["/dev/null", "/dev/zero", "/dev/urandom"] {
        argv.extend(["--dev-bind", device, device].map(OsString::from));
    }
    // Make the synthetic root read-only too: only explicit private bind mounts are writable.
    argv.extend(["--remount-ro", "/", "--seccomp", "0", "--chdir"].map(OsString::from));
    argv.push(cwd.as_os_str().to_owned());
    argv.extend(
        [
            "--",
            "/bin/sh",
            "-c",
            "exec \"$@\" </dev/null",
            "agent-ide-check",
        ]
        .map(OsString::from),
    );
    argv.push(program.as_os_str().to_owned());
    argv.extend_from_slice(args);
    Ok(Launch {
        args: argv,
        filter: socket_filter()?,
        mounts: pinned,
    })
}

/// Opens a canonical mount source without following a concurrently replaced symlink component.
/// The open file pins the object; later rename/replacement cannot substitute another host path.
fn open_mount(path: &Path) -> io::Result<File> {
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid_policy())?;
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_CLOEXEC) as u64,
        mode: 0,
        // Linux openat2 UAPI: RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS.
        resolve: 0x02 | 0x04,
    };
    // SAFETY: both pointers are live for the syscall, OpenHow matches the three-u64 Linux UAPI,
    // and the explicit size bounds kernel reads. The absolute path ignores AT_FDCWD.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat2 returned a new descriptor owned only by this File.
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}

/// Makes only pinned mounts and stdio survive exec in the forked child, never in the daemon.
/// The closure owns its Files through spawn. close_range(CLOEXEC) removes ambient descriptor
/// authority without closing the runtime's exec-error pipe before it can report a launch failure.
pub(super) fn inherit_mounts(command: &mut tokio::process::Command, mounts: Vec<Arc<File>>) {
    // SAFETY: pre_exec runs after fork; the closure allocates nothing, locks nothing, and calls
    // only close_range/fcntl on integer descriptors. No parent descriptor flags are changed.
    unsafe {
        command.pre_exec(move || {
            // Linux CLOSE_RANGE_CLOEXEC: defer closure until exec, preserving setup error reports.
            if libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 4_u32) < 0 {
                return Err(io::Error::last_os_error());
            }
            for file in &mounts {
                if libc::fcntl(file.as_raw_fd(), libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// Rejects relative and traversing paths before they can become mount destinations.
fn normal_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|part| !matches!(part, Component::ParentDir | Component::CurDir))
}

/// Rejects grants that would cover or replace a synthetic kernel/device tree.
fn reserved_root(path: &Path) -> bool {
    ["/proc", "/sys", "/dev"]
        .iter()
        .any(|reserved| path.starts_with(reserved) || Path::new(reserved).starts_with(path))
}

/// Returns a content-free policy refusal, rather than disclosing a rejected filesystem path.
fn invalid_policy() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "unrepresentable Linux check confinement policy",
    )
}

/// Produces a sealed, anonymous classic-BPF program for the native audited Linux ABI.
///
/// No socket can be created (including pathname Unix sockets inside a readable root), and
/// io_uring cannot create sockets behind the syscall filter. Foreign ABIs fail closed, including
/// x86's x32 syscall encoding. Socketpairs remain available for private child IPC; no outside
/// socket descriptor is inherited. The filter is installed by bubblewrap immediately before
/// exec and inherited by all descendants, so project code cannot remove it.
fn socket_filter() -> io::Result<File> {
    #[cfg(target_arch = "x86_64")]
    let architecture = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    let architecture = 0xc000_00b7;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "unsupported Linux seccomp architecture",
    ));

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        const LOAD_WORD: u16 = 0x20;
        const JUMP_EQUAL: u16 = 0x15;
        #[cfg(target_arch = "x86_64")]
        const JUMP_SET: u16 = 0x45;
        const RETURN: u16 = 0x06;
        const KILL_PROCESS: u32 = 0x8000_0000;
        const DENY: u32 = 0x0005_0000 | libc::EPERM as u32;
        const ALLOW: u32 = 0x7fff_0000;
        // seccomp_data: signed syscall number at offset 0, audit architecture at offset 4.
        let mut instructions = vec![
            (LOAD_WORD, 0, 0, 4),
            (JUMP_EQUAL, 1, 0, architecture),
            (RETURN, 0, 0, KILL_PROCESS),
            (LOAD_WORD, 0, 0, 0),
        ];
        #[cfg(target_arch = "x86_64")]
        instructions.extend([(JUMP_SET, 0, 1, 0x4000_0000), (RETURN, 0, 0, KILL_PROCESS)]);
        // Native x86_64/aarch64 have no socketcall multiplexor; a foreign ABI was rejected above.
        for syscall in [
            libc::SYS_socket,
            libc::SYS_connect,
            libc::SYS_io_uring_setup,
        ] {
            instructions.extend([(JUMP_EQUAL, 0, 1, syscall as u32), (RETURN, 0, 0, DENY)]);
        }
        instructions.push((RETURN, 0, 0, ALLOW));
        // Encode sock_filter's fixed Linux UAPI layout explicitly, avoiding raw struct casts.
        let mut bytes = Vec::with_capacity(instructions.len() * 8);
        for (code, yes, no, value) in instructions {
            bytes.extend_from_slice(&code.to_ne_bytes());
            bytes.extend([yes, no]);
            bytes.extend_from_slice(&value.to_ne_bytes());
        }
        // SAFETY: the name is a static NUL-terminated C string; this returns a newly owned fd.
        let fd = unsafe {
            libc::memfd_create(
                c"agent-ide-check-filter".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: memfd_create succeeded, and this is the sole owner of its new descriptor.
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(&bytes)?;
        file.rewind()?;
        // SAFETY: fcntl takes an integer seal mask, and file owns a valid memfd descriptor.
        let sealed = unsafe {
            libc::fcntl(
                file.as_raw_fd(),
                libc::F_ADD_SEALS,
                libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE,
            )
        };
        if sealed < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::seatbelt::{ReadDeny, run_confined};
    use std::{
        net::{TcpListener, TcpStream},
        os::unix::{
            fs::symlink,
            net::{UnixListener, UnixStream},
        },
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    /// One disposable fixture, removed after all its confined children have completed.
    struct Fixture {
        root: PathBuf,
        read: PathBuf,
        write: PathBuf,
        outside: PathBuf,
    }

    impl Fixture {
        /// Creates private fixture roots under the explicitly configured task temp directory.
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "lc-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            let read = root.join("r");
            let write = root.join("w");
            let outside = root.join("secret");
            std::fs::create_dir(&read).unwrap();
            std::fs::create_dir(&write).unwrap();
            std::fs::write(read.join("inside"), "allowed").unwrap();
            std::fs::write(&outside, "secret").unwrap();
            symlink(&outside, read.join("escape")).unwrap();
            Self {
                root,
                read,
                write,
                outside,
            }
        }

        /// Grants the actual test executable plus fixture roots, never the parent fixture root.
        fn policy(&self) -> SeatbeltPolicy {
            SeatbeltPolicy {
                read_roots: vec![self.read.clone(), std::env::current_exe().unwrap()],
                write_roots: vec![self.write.clone()],
                read_denies: vec![],
            }
        }

        /// Runs a selected fixture inside the real production sandbox with exact env.
        async fn probe(
            &self,
            mode: &str,
            extra: Vec<(String, String)>,
        ) -> super::super::seatbelt::ConfinedOutput {
            let mut env = vec![
                ("AGENT_IDE_LINUX_PROBE".into(), mode.into()),
                ("CHECK_READ".into(), self.read.to_str().unwrap().into()),
                ("CHECK_WRITE".into(), self.write.to_str().unwrap().into()),
                (
                    "CHECK_OUTSIDE".into(),
                    self.outside.to_str().unwrap().into(),
                ),
                ("CHECK_ALLOWED".into(), "present".into()),
            ];
            env.extend(extra);
            run_confined(
                &std::env::current_exe().unwrap(),
                &[
                    "--exact".into(),
                    "execution::linux::tests::probe_child".into(),
                    "--nocapture".into(),
                ],
                &self.read,
                &env,
                &self.policy(),
                Duration::from_secs(15),
                65536,
            )
            .await
            .unwrap()
        }
    }

    impl Drop for Fixture {
        /// Removes only this test's uniquely created files after its owned process has finished.
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }

    /// The actual sandboxed fixture program; the ordinary test invocation has no probe mode.
    #[test]
    fn probe_child() {
        let Ok(mode) = std::env::var("AGENT_IDE_LINUX_PROBE") else {
            return;
        };
        let read = PathBuf::from(std::env::var_os("CHECK_READ").unwrap());
        let write = PathBuf::from(std::env::var_os("CHECK_WRITE").unwrap());
        assert_eq!(std::env::var("CHECK_ALLOWED").unwrap(), "present");
        assert!(std::env::var_os("HOME").is_none(), "ambient HOME leaked");
        if let Ok(fd) = std::env::var("CHECK_FORBIDDEN_FD") {
            let fd: i32 = fd.parse().unwrap();
            // SAFETY: F_GETFD only queries the supplied descriptor number.
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
        }
        // Bubblewrap consumed fd 0 for seccomp; the fixed exec shim restores null stdin.
        assert_eq!(
            std::io::Read::read(&mut std::io::stdin(), &mut [0_u8; 1]).unwrap(),
            0
        );
        match mode.as_str() {
            "files" => {
                assert_eq!(
                    std::fs::read_to_string(read.join("inside")).unwrap(),
                    "allowed"
                );
                assert!(std::fs::read(std::env::var_os("CHECK_OUTSIDE").unwrap()).is_err());
                assert!(std::fs::read(read.join("escape")).is_err());
                assert!(std::fs::write(read.join("inside"), "changed").is_err());
                assert!(std::fs::write(read.join("new"), "changed").is_err());
                assert!(std::fs::rename(read.join("inside"), read.join("moved")).is_err());
                assert!(std::fs::remove_file(read.join("inside")).is_err());
                assert!(std::fs::write("/outside-write", "changed").is_err());
                std::fs::write(write.join("allowed"), "private").unwrap();
                assert_eq!(
                    std::fs::read_to_string(write.join("allowed")).unwrap(),
                    "private"
                );
            }
            "network" => {
                let address = std::env::var("CHECK_TCP").unwrap().parse().unwrap();
                assert!(TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_err());
                assert!(UnixStream::connect(read.join("socket")).is_err());
                // SAFETY: the kernel validates a null argument pointer; no userspace dereference.
                let result =
                    unsafe { libc::syscall(libc::SYS_io_uring_setup, 1, std::ptr::null::<u8>()) };
                assert_eq!(result, -1);
                assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
            }
            "tree" | "exiting-tree" => {
                // This process deliberately exits or is killed while its descendant is still
                // alive; the outer lifecycle test verifies containment and reaping afterwards.
                #[allow(clippy::zombie_processes)]
                let _child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "execution::linux::tests::probe_child",
                        "--nocapture",
                    ])
                    .env("AGENT_IDE_LINUX_PROBE", "hold")
                    .spawn()
                    .unwrap();
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while !write.join("ready").exists() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "descendant did not acquire lock"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                if mode == "tree" {
                    std::thread::sleep(Duration::from_secs(300));
                }
            }
            "hold" => {
                // SAFETY: setsid changes only this dedicated test child, not the test runner.
                assert!(unsafe { libc::setsid() } > 0);
                // SAFETY: this sets SIGTERM's disposition in the isolated probe child only.
                unsafe {
                    libc::signal(libc::SIGTERM, libc::SIG_IGN);
                }
                let lock = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(write.join("lock"))
                    .unwrap();
                // SAFETY: flock acts on the live lock descriptor owned by this child.
                assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
                std::fs::write(write.join("ready"), "ready").unwrap();
                std::thread::sleep(Duration::from_secs(300));
                drop(lock);
            }
            _ => panic!("unknown probe mode"),
        }
    }

    /// Real allowed reads/private writes and denied outside/symlink/worktree mutations coexist.
    #[tokio::test]
    async fn filesystem_and_environment_are_confined() {
        let fixture = Fixture::new();
        let secret = File::open(&fixture.outside).unwrap();
        // SAFETY: F_DUPFD duplicates this owned file at a high unused descriptor; unlike
        // try_clone, it intentionally leaves CLOEXEC unset to test ambient FD confinement.
        let fd = unsafe { libc::fcntl(secret.as_raw_fd(), libc::F_DUPFD, 200) };
        assert!(fd >= 200);
        // SAFETY: fcntl returned a newly owned fd, now closed by this guard on every exit.
        let _ambient = unsafe { File::from_raw_fd(fd) };
        let output = fixture
            .probe("files", vec![("CHECK_FORBIDDEN_FD".into(), fd.to_string())])
            .await;
        assert_eq!(output.status, Some(0), "{output:?}");
        assert_eq!(
            std::fs::read_to_string(fixture.read.join("inside")).unwrap(),
            "allowed"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.write.join("allowed")).unwrap(),
            "private"
        );
    }

    /// Host TCP and pathname Unix sockets are reachable natively but denied in the sandbox.
    #[tokio::test]
    async fn network_namespace_and_seccomp_deny_host_sockets() {
        let fixture = Fixture::new();
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let unix = UnixListener::bind(fixture.read.join("socket")).unwrap();
        let _tcp_control = TcpStream::connect(tcp.local_addr().unwrap()).unwrap();
        let _unix_control = UnixStream::connect(fixture.read.join("socket")).unwrap();
        let output = fixture
            .probe(
                "network",
                vec![("CHECK_TCP".into(), tcp.local_addr().unwrap().to_string())],
            )
            .await;
        assert_eq!(output.status, Some(0), "{output:?}");
        drop(unix);
    }

    /// An unsupported deny policy fails before executing a command with observable effects.
    #[tokio::test]
    async fn unsupported_policy_never_starts_project_code() {
        let fixture = Fixture::new();
        let mut policy = fixture.policy();
        policy
            .read_denies
            .push(ReadDeny::Path(fixture.read.join("inside")));
        let error = run_confined(
            Path::new("/bin/sh"),
            &["-c".into(), "touch marker".into()],
            &fixture.write,
            &[],
            &policy,
            Duration::from_secs(2),
            1024,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(!fixture.write.join("marker").exists());
    }

    /// Optional missing read grants do not break checks or expose objects created after prepare.
    #[tokio::test]
    async fn absent_read_grants_stay_absent_when_host_paths_appear() {
        let fixture = Fixture::new();
        let optional_directory = fixture.root.join("node_modules");
        let optional_file = fixture.root.join("package.json");
        let mut policy = fixture.policy();
        policy
            .read_roots
            .extend([optional_directory.clone(), optional_file.clone()]);
        let args = vec![
            "-c".into(),
            "test ! -e \"$1\" && test ! -e \"$2\" && cat \"$3\"".into(),
            "optional-roots".into(),
            optional_directory.clone().into_os_string(),
            optional_file.clone().into_os_string(),
            fixture.read.join("inside").into_os_string(),
        ];
        let launch = prepare(Path::new("/bin/sh"), &args, &fixture.read, &policy).unwrap();
        std::fs::create_dir(&optional_directory).unwrap();
        std::fs::write(optional_directory.join("secret"), "must remain outside").unwrap();
        std::fs::write(&optional_file, "must remain outside").unwrap();
        let mut command = tokio::process::Command::new(BWRAP);
        command
            .args(launch.args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir(&fixture.read)
            .stdin(std::process::Stdio::from(launch.filter))
            .kill_on_drop(true);
        inherit_mounts(&mut command, launch.mounts);
        let output = tokio::time::timeout(Duration::from_secs(10), command.output())
            .await
            .expect("prepared sandbox completed")
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"allowed");
    }

    /// Only a genuinely absent read grant is optional; missing writes and symlink loops refuse.
    #[test]
    fn missing_writable_roots_and_symlink_loops_still_refuse() {
        let fixture = Fixture::new();
        let mut policy = fixture.policy();
        policy.write_roots.push(fixture.root.join("missing-cache"));
        let error = prepare(Path::new("/bin/true"), &[], &fixture.read, &policy)
            .err()
            .expect("missing writable root refuses");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let loop_path = fixture.root.join("loop");
        symlink("loop", &loop_path).unwrap();
        let mut policy = fixture.policy();
        policy.read_roots.push(loop_path);
        let error = prepare(Path::new("/bin/true"), &[], &fixture.read, &policy)
            .err()
            .expect("symlink loop refuses");
        assert_eq!(error.raw_os_error(), Some(libc::ELOOP));
    }

    /// A permission failure resolving a read grant must not be mistaken for an absent grant.
    #[test]
    fn unreadable_read_root_resolution_still_refuses() {
        use std::os::unix::fs::PermissionsExt;
        // Root bypasses DAC mode bits; this contract requires the ordinary unprivileged runner.
        // SAFETY: geteuid reads the current process's effective uid and has no pointer arguments.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let fixture = Fixture::new();
        let denied = fixture.root.join("denied");
        std::fs::create_dir(&denied).unwrap();
        let path = denied.join("inside");
        std::fs::write(&path, "secret").unwrap();
        let mut policy = fixture.policy();
        policy.read_roots.push(path);
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = prepare(Path::new("/bin/true"), &[], &fixture.read, &policy);
        // Restore only the fixture mode before assertions, so Drop can remove it even on failure.
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = match result {
            Ok(_) => panic!("permission failure must refuse"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    /// A native build-script-style C compile can read system headers and write only its cache.
    #[tokio::test]
    async fn native_c_compiler_can_use_system_headers() {
        let compiler = Path::new("/usr/bin/cc");
        if !compiler.is_file() {
            eprintln!("skipping native C header check: /usr/bin/cc is not installed");
            return;
        }
        // Use the selected compiler itself, not Debian's /etc/alternatives dispatcher tree.
        let compiler = compiler.canonicalize().unwrap();
        let fixture = Fixture::new();
        let source = fixture.read.join("probe.c");
        let executable = fixture.write.join("probe");
        std::fs::write(
            &source,
            "#include <stdio.h>\nint main(void) { puts(\"headers available\"); return 0; }\n",
        )
        .unwrap();
        let env = vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("TMPDIR".into(), fixture.write.to_str().unwrap().into()),
        ];
        let output = run_confined(
            &compiler,
            &[
                source.into_os_string(),
                "-o".into(),
                executable.clone().into_os_string(),
            ],
            &fixture.read,
            &env,
            &fixture.policy(),
            Duration::from_secs(20),
            65536,
        )
        .await
        .unwrap();
        assert_eq!(output.status, Some(0), "{output:?}");
        let output = run_confined(
            &executable,
            &[],
            &fixture.read,
            &env,
            &fixture.policy(),
            Duration::from_secs(10),
            4096,
        )
        .await
        .unwrap();
        assert_eq!(output.status, Some(0), "{output:?}");
        assert_eq!(output.stdout, b"headers available\n");
    }

    /// Symlink replacement after policy preparation cannot change the pinned mount source.
    #[test]
    fn mount_sources_remain_pinned_and_filter_is_sealed() {
        use std::os::unix::fs::MetadataExt;
        let fixture = Fixture::new();
        let launch = prepare(
            Path::new("/bin/true"),
            &[],
            &fixture.read,
            &fixture.policy(),
        )
        .unwrap();
        let inode = std::fs::metadata(&fixture.read).unwrap().ino();
        std::fs::rename(&fixture.read, fixture.root.join("original")).unwrap();
        symlink(&fixture.write, &fixture.read).unwrap();
        let error = open_mount(&fixture.read).expect_err("replacement symlink is refused");
        assert_eq!(error.raw_os_error(), Some(libc::ELOOP));
        assert!(
            launch
                .mounts
                .iter()
                .any(|file| file.metadata().unwrap().ino() == inode)
        );
        assert!(std::io::Write::write(&mut &launch.filter, b"changed").is_err());
    }

    /// Waits for a real descendant-held kernel lock, not a guessed startup delay.
    async fn ready_lock(fixture: &Fixture) -> File {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !fixture.write.join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("sandboxed descendant reached readiness");
        File::open(fixture.write.join("lock")).unwrap()
    }

    /// Proves a descendant no longer holds its kernel resource, including a child using setsid.
    async fn assert_lock_released(lock: &File) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // SAFETY: lock owns a live descriptor; a nonblocking flock cannot hang.
                if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    break;
                }
                assert_eq!(
                    io::Error::last_os_error().raw_os_error(),
                    Some(libc::EWOULDBLOCK)
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("TERM-resistant setsid descendant was killed");
    }

    /// Normal leader exit kills even session-detached descendants inside the private PID namespace.
    #[tokio::test]
    async fn exiting_leader_takes_detached_descendant_along() {
        let fixture = Fixture::new();
        let output = fixture.probe("exiting-tree", vec![]).await;
        assert_eq!(output.status, Some(0), "{output:?}");
        let lock = ready_lock(&fixture).await;
        assert_lock_released(&lock).await;
    }

    /// Cancelling the admitted caller keeps ownership until its namespace and child are reaped.
    #[tokio::test]
    async fn cancellation_reaps_namespace_before_releasing_admission() {
        use super::super::{AdmissionController, AdmissionLimits, OwnerId};
        let fixture = Fixture::new();
        let admission = Arc::new(std::sync::Mutex::new(
            AdmissionController::new(AdmissionLimits {
                total_running: 1,
                per_owner_running: 1,
                per_owner_queued: 1,
                total_queued: 1,
                interactive_burst: 1,
            })
            .unwrap(),
        ));
        let handle = tokio::spawn({
            let admission = admission.clone();
            let cwd = fixture.read.clone();
            let writable = fixture.write.clone();
            let policy = fixture.policy();
            async move {
                super::super::seatbelt::run_admitted(
                    &admission,
                    OwnerId::new("cancel-test").unwrap(),
                    &std::env::current_exe().unwrap(),
                    &[
                        "--exact".into(),
                        "execution::linux::tests::probe_child".into(),
                        "--nocapture".into(),
                    ],
                    &cwd,
                    &[
                        ("AGENT_IDE_LINUX_PROBE".into(), "tree".into()),
                        ("CHECK_READ".into(), cwd.to_str().unwrap().into()),
                        ("CHECK_WRITE".into(), writable.to_str().unwrap().into()),
                        ("CHECK_ALLOWED".into(), "present".into()),
                    ],
                    Some(&policy),
                    Duration::from_secs(30),
                    4096,
                )
                .await
            }
        });
        let lock = ready_lock(&fixture).await;
        // SAFETY: a nonblocking probe of our live descriptor establishes the child holds its lock.
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::EWOULDBLOCK)
        );
        assert_eq!(admission.lock().unwrap().running_count(), 1);
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(10), async {
            while admission.lock().unwrap().running_count() != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled job was reaped and its slot released");
        assert_lock_released(&lock).await;
    }

    /// A real timeout kills the private PID namespace; bounded output still drains to completion.
    #[tokio::test]
    async fn timeout_and_output_cap_remain_bounded() {
        let fixture = Fixture::new();
        let output = run_confined(
            Path::new("/bin/sh"),
            &["-c".into(), "trap '' TERM; printf ready; sleep 300".into()],
            &fixture.read,
            &[("PATH".into(), "/usr/bin:/bin".into())],
            &fixture.policy(),
            Duration::from_secs(2),
            128,
        )
        .await
        .unwrap();
        assert!(output.timed_out, "{output:?}");
        assert_eq!(output.stdout, b"ready");
        let output = run_confined(
            Path::new("/bin/sh"),
            &["-c".into(), "head -c 100000 /dev/zero".into()],
            &fixture.read,
            &[("PATH".into(), "/usr/bin:/bin".into())],
            &fixture.policy(),
            Duration::from_secs(10),
            128,
        )
        .await
        .unwrap();
        assert_eq!(output.status, Some(0), "{output:?}");
        assert!(output.truncated);
        assert_eq!(output.stdout.len(), 128);
    }
}
