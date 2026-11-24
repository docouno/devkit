//! Darwin has no parent-death signal or child subreaper. Watch the invoking
//! process in-process and arm an independent observer before a creator may exec.
use super::*;
use crate::state::proc_stat_ppid_and_start;
use std::{
    ffi::{OsStr, OsString},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    path::PathBuf,
    ptr,
    sync::OnceLock,
};

pub const PARENT_HANDOFF: &str = "DEVKIT_EXEC_PARENT";
static PARENT: OnceLock<(i32, u64)> = OnceLock::new();

struct Queue(OwnedFd);

impl Queue {
    fn new() -> io::Result<Self> {
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let queue = Self(unsafe { OwnedFd::from_raw_fd(fd) });
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(queue)
    }

    fn register(&self, id: i32, filter: i16, flags: u16, fflags: u32) -> io::Result<()> {
        let event = libc::kevent {
            ident: id as usize,
            filter,
            flags,
            fflags,
            data: 0,
            udata: ptr::null_mut(),
        };
        loop {
            let n = unsafe {
                libc::kevent(
                    self.0.as_raw_fd(),
                    &event,
                    1,
                    ptr::null_mut(),
                    0,
                    ptr::null(),
                )
            };
            if n >= 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    fn watch_exit(&self, pid: i32) -> io::Result<()> {
        self.register(
            pid,
            libc::EVFILT_PROC,
            libc::EV_ADD | libc::EV_ONESHOT,
            libc::NOTE_EXIT,
        )
    }

    fn wait(&self) -> io::Result<libc::kevent> {
        let mut result = std::mem::MaybeUninit::uninit();
        loop {
            let n = unsafe {
                libc::kevent(
                    self.0.as_raw_fd(),
                    ptr::null(),
                    0,
                    result.as_mut_ptr(),
                    1,
                    ptr::null(),
                )
            };
            if n == 1 {
                let event = unsafe { result.assume_init() };
                if event.flags & libc::EV_ERROR != 0 {
                    return Err(io::Error::from_raw_os_error(event.data as i32));
                }
                return Ok(event);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

fn start_of(pid: i32) -> Option<u64> {
    proc_stat_ppid_and_start(pid.try_into().ok()?).map(|(_, start)| start)
}

pub fn watch_cli_parent(runner: &ProcessRunner, handoff: Option<&OsStr>) -> Result<()> {
    let pid = unsafe { libc::getppid() };
    let started = start_of(pid).unwrap_or(0);
    if let Some(handoff) = handoff {
        let expected = format!("{pid}:{started}");
        if handoff != OsStr::new(&expected) {
            runner.cancel(libc::SIGTERM);
            return runner.check_cancelled();
        }
    }
    PARENT
        .set((pid, started))
        .map_err(|_| anyhow::anyhow!("Parent already registered"))?;
    let queue = Queue::new()?;
    let events = match queue.watch_exit(pid) {
        Ok(()) => Some(queue),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => None,
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {
            runner.cancel(libc::SIGTERM);
            return runner.check_cancelled();
        }
        Err(error) => return Err(error.into()),
    };
    if unsafe { libc::getppid() } != pid || (started != 0 && start_of(pid) != Some(started)) {
        runner.cancel(libc::SIGTERM);
        return runner.check_cancelled();
    }
    let signal = runner.signals.cancelled.clone();
    thread::Builder::new()
        .name("devkit-parent".into())
        .spawn(move || {
            if let Some(queue) = events {
                // Parent exit or an unreadable queue both revoke the parent relationship.
                let _ = queue.wait();
            } else {
                while unsafe { libc::getppid() } == pid {
                    thread::sleep(Duration::from_millis(100));
                }
            }
            let _ = signal.compare_exchange(
                0,
                libc::SIGTERM as usize,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        })?;
    Ok(())
}

pub(super) fn set_handoff(command: &mut Command) -> Result<()> {
    if let Some((pid, started)) = PARENT.get() {
        ensure!(
            unsafe { libc::getppid() } == *pid,
            "Parent exited before delegation"
        );
        command.env(PARENT_HANDOFF, format!("{pid}:{started}"));
    }
    Ok(())
}

unsafe extern "C" {
    fn devkit_main_image_header() -> usize;
    fn devkit_image_identity_at(pid: i32, header: usize, device: *mut u32, inode: *mut u64) -> i32;
}

pub(super) struct WatcherImage {
    path: PathBuf,
    file: std::fs::File,
}

impl WatcherImage {
    pub(super) fn open(path: PathBuf) -> Result<Self> {
        let path = std::fs::canonicalize(path)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        ensure!(file.metadata()?.is_file(), "Invalid observer executable");
        Ok(Self { path, file })
    }

    pub(super) fn current() -> Result<Self> {
        let image = Self::open(std::env::current_exe()?)?;
        image.check_mapping(std::process::id() as i32, unsafe {
            devkit_main_image_header()
        })?;
        Ok(image)
    }

    fn check_path(&self) -> Result<()> {
        let held = self.file.metadata()?;
        let path = std::fs::symlink_metadata(&self.path)?;
        ensure!(
            path.is_file() && path.dev() == held.dev() && path.ino() == held.ino(),
            "Observer executable changed"
        );
        Ok(())
    }

    fn check_mapping(&self, pid: i32, header: usize) -> Result<()> {
        let (mut device, mut inode) = (0, 0);
        let error = unsafe { devkit_image_identity_at(pid, header, &mut device, &mut inode) };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error))
                .context("Cannot identify mapped observer executable");
        }
        let held = self.file.metadata()?;
        ensure!(
            (u64::from(device), inode) == (held.dev(), held.ino()),
            "Mapped observer executable changed"
        );
        self.check_path()
    }
}

const READY: u8 = 1;
const TERMINATE: u8 = 2;

/// Only the observer signals the group. Its own child reserves the group ID
/// until a membership snapshot proves that no live producers remain.
pub(super) struct Observer {
    child: Child,
    channel: UnixStream,
    pgid: i32,
    waited: bool,
    admitted: bool,
    startup_deadline: Instant,
}

fn reap(pid: i32) {
    while unsafe { libc::waitpid(pid, ptr::null_mut(), 0) } < 0
        && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
    {}
}

// Transfer a pending wait past its caller's deadline. The standalone case also
// transfers the group: the direct child's PID pins its identity until reap().
fn reap_later(pid: i32, group: Option<i32>) {
    let cleanup = move || {
        if let Some(pgid) = group {
            drain_until(pgid, None);
        }
        reap(pid);
    };
    // Thread limits can prevent the handoff; keep ownership in this thread.
    if thread::Builder::new()
        .name("devkit-reap".into())
        .spawn(cleanup)
        .is_err()
    {
        cleanup();
    }
}

fn wait_before(child: &mut Child, deadline: Instant) -> Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            reap_later(child.id() as i32, None);
            anyhow::bail!("Creator process group cleanup exceeded its deadline");
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn bound_handshake(channel: &UnixStream, deadline: Instant) -> Result<()> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|t| !t.is_zero())
        .context("Creator group observer startup exceeded its deadline")?;
    channel.set_read_timeout(Some(remaining))?;
    channel.set_write_timeout(Some(remaining))?;
    Ok(())
}

fn read_handshake(channel: &mut UnixStream, mut frame: &mut [u8], deadline: Instant) -> Result<()> {
    // SO_RCVTIMEO bounds each read. Partial frames share the startup deadline.
    while !frame.is_empty() {
        bound_handshake(channel, deadline)?;
        match channel.read(frame) {
            Ok(0) => anyhow::bail!("Observer closed its startup socket"),
            Ok(count) => frame = &mut frame[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

impl Observer {
    fn start(image: &WatcherImage, startup_deadline: Instant) -> Result<Self> {
        image.check_path()?;
        let (channel, peer) = UnixStream::pair()?;
        let fd = peer.as_raw_fd();
        let mut command = Command::new(&image.path);
        command
            .args(["internal", "watch-group"])
            .arg(std::process::id().to_string())
            .arg(fd.to_string())
            .env_remove(PARENT_HANDOFF)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .context("Cannot start creator group observer")?;
        drop(peer);
        let mut observer = Self {
            child,
            channel,
            pgid: 0,
            waited: false,
            admitted: false,
            startup_deadline,
        };
        let mut header = [0; size_of::<usize>()];
        read_handshake(&mut observer.channel, &mut header, startup_deadline)?;
        image.check_mapping(observer.child.id() as i32, usize::from_ne_bytes(header))?;
        bound_handshake(&observer.channel, startup_deadline)?;
        observer.channel.write_all(&[READY])?;
        observer.admitted = true;
        let mut bytes = [0; 4];
        read_handshake(&mut observer.channel, &mut bytes, startup_deadline)
            .context("Creator group observer failed to initialize")?;
        observer.pgid = i32::from_ne_bytes(bytes);
        ensure!(observer.pgid > 1, "Invalid creator group acknowledgement");
        Ok(observer)
    }

    fn finish(&mut self, deadline: Instant) -> Result<()> {
        // EOF also handles exec, controller death, and failures during setup.
        let _ = self.channel.shutdown(std::net::Shutdown::Both);
        if !self.admitted {
            let _ = self.child.kill();
        }
        self.waited = true;
        let status = wait_before(&mut self.child, deadline)?;
        ensure!(status.success(), "Creator group observer failed");
        Ok(())
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        if !self.waited {
            let _ = self.finish(
                self.startup_deadline
                    .min(Instant::now() + Duration::from_secs(1)),
            );
        }
    }
}

pub(super) fn spawn(
    command: &mut Command,
    executable: Option<&WatcherImage>,
    deadline: Option<Instant>,
) -> Result<Creator> {
    // Auto-reaped children cannot pin a numeric PGID or supply an exit status.
    let mut action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
    if unsafe { libc::sigaction(libc::SIGCHLD, ptr::null(), action.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let action = unsafe { action.assume_init() };
    ensure!(
        action.sa_sigaction != libc::SIG_IGN && action.sa_flags & libc::SA_NOCLDWAIT == 0,
        "Creator supervision requires waitable children"
    );
    let Some(executable) = executable else {
        return Ok(Creator::new(command.spawn()?));
    };
    let maximum = Instant::now() + Duration::from_secs(5);
    let startup_deadline = deadline.map_or(maximum, |at| at.min(maximum));
    let observer = Observer::start(executable, startup_deadline)?;
    bound_handshake(&observer.channel, startup_deadline)?;
    let pgid = observer.pgid;
    let owner = std::process::id() as i32;
    let fd = observer.channel.as_raw_fd();
    command.process_group(pgid);
    // A Command retains pre_exec callbacks across spawns. Retire ours before
    // the descriptor can be closed and reused in the controller.
    let active = Arc::new(AtomicBool::new(true));
    let armed = active.clone();
    unsafe {
        command.pre_exec(move || {
            if armed.load(Ordering::Acquire) {
                if libc::getppid() != owner || libc::getpgrp() != pgid {
                    return Err(io::Error::from_raw_os_error(libc::EPERM));
                }
                // An earlier pre_exec callback may have closed descriptors.
                // Only borrow this socket number through async-signal-safe calls.
                let bytes = libc::getpid().to_ne_bytes();
                transfer(bytes.len(), || {
                    libc::send(fd, bytes.as_ptr().cast(), bytes.len(), libc::MSG_NOSIGNAL)
                })?;
                let mut ack = 0_u8;
                transfer(1, || libc::read(fd, (&raw mut ack).cast(), 1))?;
                if ack != READY {
                    return Err(io::Error::from_raw_os_error(libc::EPIPE));
                }
            }
            Ok(())
        });
    }
    let result = command.spawn();
    active.store(false, Ordering::Release);
    let mut creator = Creator::new(result?);
    creator.pid = pgid;
    creator.observer = Some(observer);
    Ok(creator)
}

// A short control frame aborts this spawn before exec. The controller then
// owns both children and tears down the observer's group.
fn transfer(size: usize, mut syscall: impl FnMut() -> isize) -> io::Result<()> {
    loop {
        let n = syscall();
        if n == size as isize {
            return Ok(());
        }
        if n >= 0 {
            return Err(io::Error::from_raw_os_error(libc::EPIPE));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Observe status without collecting the direct child. In the standalone
/// library path that child itself pins the group's numeric identity.
pub(super) fn poll_creator(child: &Child) -> io::Result<Option<ExitStatus>> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            if info.si_pid == 0 {
                return Ok(None);
            }
            let status = match info.si_code {
                libc::CLD_EXITED => (info.si_status & 255) << 8,
                libc::CLD_KILLED => info.si_status,
                libc::CLD_DUMPED => info.si_status | 0x80,
                _ => return Err(io::Error::from_raw_os_error(libc::EINVAL)),
            };
            return Ok(Some(ExitStatus::from_raw(status)));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn members(pgid: i32) -> io::Result<Vec<i32>> {
    let mut pids = vec![0; 8];
    loop {
        let bytes = i32::try_from(size_of_val(pids.as_slice()))
            .map_err(|_| io::Error::from_raw_os_error(libc::EOVERFLOW))?;
        let n = unsafe {
            *libc::__error() = 0;
            libc::proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), bytes)
        };
        if n <= 0 {
            let error = io::Error::last_os_error();
            if n < 0 || error.raw_os_error() != Some(0) {
                return Err(error);
            }
        }
        if (n as usize) < pids.len() {
            pids.truncate(n as usize);
            return Ok(pids);
        }
        pids.resize(pids.len() * 2, 0);
    }
}

fn zombie_birth(pid: i32) -> Option<(u64, u64)> {
    let info = crate::state::darwin_bsd_info(pid, true).ok()?;
    (info.pbi_pid == pid as u32 && info.pbi_status == libc::SZOMB)
        .then_some((info.pbi_start_tvsec, info.pbi_start_tvusec))
}

fn drained(pgid: i32) -> io::Result<bool> {
    let mut zombies = BTreeMap::new();
    for pid in members(pgid)? {
        let Some(birth) = zombie_birth(pid) else {
            return Ok(false);
        };
        zombies.insert(pid, birth);
    }

    if zombies.is_empty() {
        return Ok(true);
    }
    // Check the identities on both sides of a fresh kernel group snapshot.
    // A process that forked and died during the first inspection may have left
    // a new member. A zombie held by another parent can never fork again.
    Ok(members(pgid)?.into_iter().all(|pid| {
        zombies
            .get(&pid)
            .is_some_and(|birth| Some(*birth) == zombie_birth(pid))
    }))
}

fn drain_until(pgid: i32, deadline: Option<Instant>) -> bool {
    // killpg targets a snapshot. Keep the identity pinned and repeat until a
    // fresh snapshot proves that no ordinary descendant can fork again.
    let mut delay = Duration::from_millis(2);
    loop {
        signal_group(pgid, libc::SIGKILL);
        if matches!(drained(pgid), Ok(true)) {
            return true;
        }
        let wait = deadline.map_or(delay, |at| {
            at.saturating_duration_since(Instant::now()).min(delay)
        });
        if wait.is_zero() {
            return false;
        }
        thread::sleep(wait);
        delay = (delay * 2).min(Duration::from_millis(100));
    }
}

struct Anchor {
    pid: i32,
    _lifetime: io::PipeWriter,
}

impl Anchor {
    fn start(channel: &UnixStream, queue: &Queue) -> Result<Self> {
        let (reader, writer) = io::pipe()?;
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if pid == 0 {
            // No allocations, runtime code or inherited locks after fork.
            unsafe {
                libc::close(channel.as_raw_fd());
                libc::close(queue.0.as_raw_fd());
                libc::close(writer.as_raw_fd());
                // The pipe closes on observer exit, including failure before enrollment.
                let mut byte = 0u8;
                while libc::read(reader.as_raw_fd(), (&raw mut byte).cast(), 1) < 0
                    && *libc::__error() == libc::EINTR
                {}
                libc::_exit(0);
            }
        }
        let anchor = Self {
            pid,
            _lifetime: writer,
        };
        if unsafe { libc::setpgid(pid, pid) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(anchor)
    }
}

impl Drop for Anchor {
    fn drop(&mut self) {
        // Also covers failure to create the group before enrollment.
        unsafe { libc::kill(self.pid, libc::SIGKILL) };
        drain_until(self.pid, None);
        reap(self.pid);
    }
}

impl Creator {
    pub(super) fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        if let Some(observer) = &mut self.observer {
            // The observer may already be draining after the task exited.
            let _ = observer.channel.write_all(&[TERMINATE]);
        } else {
            signal_group(self.pid, libc::SIGTERM);
            signal_group(self.pid, libc::SIGCONT);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while matches!(self.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        self.finish_metadata(Instant::now() + Duration::from_secs(1))
    }

    pub(super) fn finish_metadata(&mut self, deadline: Instant) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        let _ = self.child.kill();
        self.finished = true;
        let stopped = if let Some(mut observer) = self.observer.take() {
            observer.finish(deadline)
        } else if drain_until(self.pid, Some(deadline)) {
            Ok(())
        } else {
            reap_later(self.child.id() as i32, Some(self.pid));
            anyhow::bail!("Creator process group cleanup exceeded its deadline");
        };
        let waited = wait_before(&mut self.child, deadline);
        stopped?;
        waited?;
        Ok(())
    }
}

pub fn run_watcher(args: &[OsString]) -> Result<()> {
    ensure!(
        args.len() == 2,
        "Expected controller PID and observer socket"
    );
    let parse = |arg: &OsString| -> Result<i32> {
        Ok(arg.to_str().context("Invalid observer argument")?.parse()?)
    };
    let owner = parse(&args[0])?;
    let fd = parse(&args[1])?;
    ensure!(
        owner > 1
            && unsafe { libc::getppid() == owner && libc::getpgrp() == libc::getpid() }
            && fd >= 3,
        "Invalid observer parent"
    );
    let mut kind: libc::c_int = 0;
    let mut size = size_of_val(&kind) as libc::socklen_t;
    ensure!(
        unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&raw mut kind).cast(),
                &mut size,
            )
        } == 0
            && kind == libc::SOCK_STREAM,
        "Invalid observer socket"
    );
    // Post-exec and single-threaded. /dev/fd can enumerate its own transient directory FD.
    let mut descriptors = Vec::new();
    for entry in std::fs::read_dir("/dev/fd")? {
        let name = entry?.file_name();
        let n: i32 = name.to_str().context("Invalid descriptor name")?.parse()?;
        if n >= 3 && n != fd {
            descriptors.push(n);
        }
    }
    for n in descriptors {
        if unsafe { libc::close(n) } < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EBADF) {
                return Err(error.into());
            }
        }
    }
    let mut channel = unsafe { UnixStream::from_raw_fd(fd) };
    channel.set_read_timeout(Some(Duration::from_secs(5)))?;
    channel.set_write_timeout(Some(Duration::from_secs(5)))?;
    let queue = Queue::new()?;
    queue.watch_exit(owner)?;
    queue.register(fd, libc::EVFILT_READ, libc::EV_ADD, 0)?;
    ensure!(
        unsafe { libc::getppid() } == owner,
        "Observer parent exited"
    );
    // A terminal or graceful group shutdown must leave the observer available
    // to finish the job if its controller is killed midway through cleanup.
    for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
        ensure!(
            unsafe { libc::signal(sig, libc::SIG_IGN) } != libc::SIG_ERR,
            "Cannot arm group observer"
        );
    }
    // The controller checks this child's actual VM region against its open
    // executable before admitting the observer or starting a creator.
    channel.write_all(&unsafe { devkit_main_image_header() }.to_ne_bytes())?;
    let mut admitted = [0];
    channel.read_exact(&mut admitted)?;
    ensure!(admitted == [READY], "Observer executable was not admitted");
    let anchor = Anchor::start(&channel, &queue)?;
    channel.write_all(&anchor.pid.to_ne_bytes())?;
    if queue.wait()?.filter == libc::EVFILT_READ {
        let mut bytes = [0; 4];
        if let Err(error) = channel.read_exact(&mut bytes) {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                return Ok(());
            }
            return Err(error.into());
        }
        let pid = i32::from_ne_bytes(bytes);
        ensure!(
            pid > 1 && unsafe { libc::getpgid(pid) } == anchor.pid,
            "Invalid creator group"
        );
        queue.watch_exit(pid)?;
        channel.write_all(&[READY])?;
        while queue.wait()?.filter == libc::EVFILT_READ {
            let mut action = [0];
            if channel.read(&mut action)? == 0 {
                break;
            }
            ensure!(action == [TERMINATE], "Invalid creator group request");
            signal_group(anchor.pid, libc::SIGTERM);
            signal_group(anchor.pid, libc::SIGCONT);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_observer_frame_shares_one_deadline() {
        let (mut receiver, mut sender) = UnixStream::pair().unwrap();
        let writer = thread::spawn(move || {
            for _ in 0..8 {
                if sender.write_all(&[READY]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(60));
            }
        });
        assert!(
            read_handshake(
                &mut receiver,
                &mut [0; 8],
                Instant::now() + Duration::from_millis(200)
            )
            .is_err()
        );
        drop(receiver);
        writer.join().unwrap();
    }

    #[test]
    fn wrong_mapped_image_fails_observer_admission() {
        let header = unsafe { devkit_main_image_header() };
        let pid = std::process::id() as i32;
        WatcherImage::current()
            .unwrap()
            .check_mapping(pid, header)
            .unwrap();
        let other = WatcherImage::open("/bin/sh".into()).unwrap();
        assert!(other.check_mapping(pid, header).is_err());
    }

    #[test]
    fn observing_normal_exit_keeps_the_standalone_leader_waitable_until_cleanup() {
        let mut creator = spawn(
            Command::new("sh").args(["-c", "exit 37"]).process_group(0),
            None,
            None,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = creator.try_wait().unwrap() {
                assert_eq!(status.code(), Some(37));
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        // A second kernel observation succeeds only while the leader is held.
        assert_eq!(
            poll_creator(&creator.child).unwrap().unwrap().code(),
            Some(37)
        );
        creator.finish().unwrap();
        assert_eq!(
            poll_creator(&creator.child).unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
        creator.finish().unwrap();
    }
}
