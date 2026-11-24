//! Darwin has no parent-death signal or child subreaper. Watch the invoking
//! process in-process and arm an independent observer before a creator may exec.
use super::*;
use crate::state::proc_stat_ppid_and_start;
use std::{
    ffi::{OsStr, OsString},
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    path::Path,
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

const READY: u8 = 1;

/// Each active creator has a group leader independent of the task. The caller
/// owns this direct child and never collects it before the last group signal.
pub(super) struct Observer {
    child: Child,
    channel: UnixStream,
}

impl Observer {
    fn start(executable: &Path) -> Result<Self> {
        let (channel, peer) = UnixStream::pair()?;
        channel.set_read_timeout(Some(Duration::from_secs(5)))?;
        channel.set_write_timeout(Some(Duration::from_secs(5)))?;
        let fd = peer.as_raw_fd();
        let mut command = Command::new(executable);
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
        let mut observer = Self { child, channel };
        let mut ack = [0];
        observer
            .channel
            .read_exact(&mut ack)
            .context("Creator group observer failed to initialize")?;
        ensure!(ack == [READY], "Invalid creator group acknowledgement");
        Ok(observer)
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        // Our unreaped direct child pins this PID, even if its program exited.
        signal_group(self.child.id() as i32, libc::SIGKILL);
        let _ = self.child.wait();
    }
}

pub(super) fn spawn(command: &mut Command, executable: Option<&Path>) -> Result<Creator> {
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
    let observer = Observer::start(executable)?;
    let pgid = observer.child.id() as i32;
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

fn descendants_remain(pgid: i32, task: i32) -> io::Result<bool> {
    // At most two known direct children can occupy the group. Three slots
    // suffice to find another member, including an orphan awaiting launchd.
    let mut pids = [0i32; 3];
    let n = unsafe {
        *libc::__error() = 0;
        libc::proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), size_of_val(&pids) as i32)
    };
    if n <= 0 {
        let error = io::Error::last_os_error();
        if n < 0 || error.raw_os_error() != Some(0) {
            return Err(error);
        }
    }
    Ok(pids[..n as usize]
        .iter()
        .any(|pid| *pid != pgid && *pid != task))
}

impl Creator {
    pub(super) fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        signal_group(self.pid, libc::SIGTERM);
        signal_group(self.pid, libc::SIGCONT);
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
        signal_group(self.pid, libc::SIGKILL);
        // The direct executable may have moved itself to a different group.
        // Its PID is still ours until wait(), and a second SIGKILL is harmless.
        let _ = self.child.kill();
        // Stop every member before giving up ownership of the group identity.
        let drained = (|| -> Result<()> {
            loop {
                if self.try_wait()?.is_some()
                    && !descendants_remain(self.pid, self.child.id() as i32)?
                {
                    return Ok(());
                }
                ensure!(
                    Instant::now() < deadline,
                    "Creator process group was not reaped"
                );
                thread::sleep(Duration::from_millis(2));
            }
        })();
        self.finished = true;
        let waited = self.child.wait();
        drop(self.observer.take());
        drained?;
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
    // From READY until exit we are the leader. On every return path signal our
    // own group, even when both the task and its controller have been reaped.
    struct OwnGroup;
    impl Drop for OwnGroup {
        fn drop(&mut self) {
            unsafe {
                libc::kill(0, libc::SIGKILL);
            }
        }
    }
    let _group = OwnGroup;
    channel.write_all(&[READY])?;
    if queue.wait()?.filter == libc::EVFILT_READ {
        let mut bytes = [0; 4];
        channel.read_exact(&mut bytes)?;
        let pid = i32::from_ne_bytes(bytes);
        ensure!(
            pid > 1 && unsafe { libc::getpgid(pid) == libc::getpgrp() },
            "Invalid creator group"
        );
        queue.watch_exit(pid)?;
        channel.write_all(&[READY])?;
        // NOTE_EXIT for either process, or EOF on the controller's CLOEXEC
        // socket during image replacement, ends this group's lifetime.
        queue.wait()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observing_normal_exit_keeps_the_standalone_leader_waitable_until_cleanup() {
        let mut creator = spawn(
            Command::new("sh").args(["-c", "exit 37"]).process_group(0),
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
