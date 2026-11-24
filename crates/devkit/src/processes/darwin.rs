//! Darwin has no parent-death signal or child subreaper. Watch the invoking
//! process in-process and arm an independent observer before a creator may exec.
use super::*;
use crate::state::proc_stat_ppid_and_start;
use anyhow::bail;
use std::{
    ffi::{OsStr, OsString},
    os::{
        fd::{FromRawFd, OwnedFd, RawFd},
        unix::net::UnixStream,
    },
    path::PathBuf,
    ptr,
    sync::{Mutex, OnceLock},
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

const ARM: u8 = 1;
const DISARM: u8 = 2;
const EXIT: u8 = 3;
const ACK: u8 = 1;

struct Observer {
    child: Child,
    channel: UnixStream,
}

impl Observer {
    fn start(executable: &std::path::Path) -> Result<Self> {
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
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: only async-signal-safe syscalls between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 || libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
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
        observer
            .ack()
            .context("Creator group observer failed to initialize")?;
        Ok(observer)
    }

    fn ack(&mut self) -> Result<()> {
        let mut ack = [0];
        self.channel.read_exact(&mut ack)?;
        ensure!(ack == [ACK], "Invalid creator group acknowledgement");
        Ok(())
    }

    fn send(&mut self, action: u8, pid: i32) -> Result<()> {
        let mut message = [action; 5];
        message[1..].copy_from_slice(&pid.to_ne_bytes());
        self.channel.write_all(&message)?;
        self.ack()
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        // EOF or the controller's NOTE_EXIT also causes the observer to exit.
        let _ = self.send(EXIT, 0);
        let _ = self.channel.shutdown(std::net::Shutdown::Both);
        let deadline = Instant::now() + Duration::from_millis(500);
        while self.child.try_wait().is_ok_and(|status| status.is_none())
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[derive(Default)]
pub(super) struct GroupWatcher {
    executable: OnceLock<PathBuf>,
    observer: Mutex<Option<Observer>>,
}

impl GroupWatcher {
    pub(super) fn enable(&self, path: PathBuf) -> Result<()> {
        self.executable
            .set(path)
            .map_err(|_| anyhow::anyhow!("Group observer already enabled"))
    }

    pub(super) fn shutdown(&self) -> Result<()> {
        let mut guard = self
            .observer
            .lock()
            .map_err(|_| anyhow::anyhow!("Group observer lock failed"))?;
        drop(guard.take());
        Ok(())
    }

    pub(super) fn disarm(&self, pid: i32) -> Result<()> {
        let mut guard = self
            .observer
            .lock()
            .map_err(|_| anyhow::anyhow!("Group observer lock failed"))?;
        if let Some(observer) = guard.as_mut() {
            observer.send(DISARM, pid)?;
        }
        Ok(())
    }

    pub(super) fn spawn(&self, command: &mut Command) -> Result<Child> {
        let Some(executable) = self.executable.get() else {
            return Ok(command.spawn()?);
        };
        let mut guard = self
            .observer
            .lock()
            .map_err(|_| anyhow::anyhow!("Group observer lock failed"))?;
        if guard.is_none() {
            *guard = Some(Observer::start(executable)?);
        }
        let observer = guard.as_mut().unwrap();
        let (mut pid_rd, pid_wr) = io::pipe()?;
        let (gate_rd, mut gate_wr) = io::pipe()?;
        let fds = [
            pid_rd.as_raw_fd(),
            pid_wr.as_raw_fd(),
            gate_rd.as_raw_fd(),
            gate_wr.as_raw_fd(),
        ];
        // Command can be reused. Disable this callback before releasing our descriptors.
        let active = Arc::new(AtomicBool::new(true));
        let armed = active.clone();
        unsafe {
            command.pre_exec(move || {
                if armed.load(Ordering::Acquire) {
                    child_gate(fds)?;
                }
                Ok(())
            });
        }
        let (result, ack) = thread::scope(|scope| -> Result<_> {
            let coordinator = thread::Builder::new()
                .name("devkit-arm".into())
                .spawn_scoped(scope, move || -> Result<i32> {
                    let mut bytes = [0; 4];
                    pid_rd.read_exact(&mut bytes)?;
                    let pid = i32::from_ne_bytes(bytes);
                    observer.send(ARM, pid)?;
                    if let Err(error) = gate_wr.write_all(&[ACK]) {
                        let _ = observer.send(DISARM, pid);
                        return Err(error.into());
                    }
                    Ok(pid)
                });
            let coordinator = match coordinator {
                Ok(handle) => handle,
                Err(error) => {
                    active.store(false, Ordering::Release);
                    return Err(error.into());
                }
            };
            let result = command.spawn();
            active.store(false, Ordering::Release);
            drop(pid_wr);
            drop(gate_rd);
            let ack = coordinator
                .join()
                .map_err(|_| anyhow::anyhow!("Group observer coordinator failed"))?;
            Ok((result, ack))
        })?;
        match (result, ack) {
            (Ok(child), Ok(_)) => Ok(child),
            (Err(error), Ok(pid)) => {
                guard.as_mut().unwrap().send(DISARM, pid)?;
                Err(error.into())
            }
            (Err(error), Err(arming)) => {
                Err(error).context(format!("Cannot arm creator group: {arming:#}"))
            }
            (Ok(mut child), Err(error)) => {
                signal_group(child.id() as i32, libc::SIGKILL);
                let _ = child.wait();
                Err(error)
            }
        }
    }
}

/// Observe exit before reaping so the direct child's PID still pins its PGID
/// while we stop any remaining descendants. launchd reaps orphans on Darwin.
pub(super) fn poll_creator(child: &mut Child) -> io::Result<Option<ExitStatus>> {
    let pid = child.id();
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            if info.si_pid == 0 {
                return Ok(None);
            }
            signal_group(pid as i32, libc::SIGKILL);
            return child.try_wait();
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ECHILD) {
            return child.try_wait();
        }
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

// Executed in a fork child. Return raw errno so Rust's exec-error pipe reports
// failure and std::process reaps the child; never allocate or unwind here.
fn child_gate([pid_rd, pid_wr, gate_rd, gate_wr]: [RawFd; 4]) -> io::Result<()> {
    unsafe {
        libc::close(pid_rd);
        libc::close(gate_wr);
    }
    let bytes = unsafe { libc::getpid() }.to_ne_bytes();
    let result = loop {
        let n = unsafe { libc::write(pid_wr, bytes.as_ptr().cast(), bytes.len()) };
        if n == bytes.len() as isize {
            break Ok(());
        }
        if n >= 0 {
            break Err(io::Error::from_raw_os_error(libc::EPIPE));
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            break Err(err);
        }
    };
    unsafe {
        libc::close(pid_wr);
    }
    let result = result.and_then(|()| {
        let mut byte = 0_u8;
        loop {
            let n = unsafe { libc::read(gate_rd, (&raw mut byte).cast(), 1) };
            if n == 1 && byte == ACK {
                return Ok(());
            }
            if n >= 0 {
                return Err(io::Error::from_raw_os_error(libc::EPIPE));
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                return Err(err);
            }
        }
    });
    unsafe {
        libc::close(gate_rd);
    }
    result
}

#[derive(Default)]
struct Groups(BTreeMap<i32, u64>);

impl Groups {
    fn arm(&mut self, queue: &Queue, pid: i32, owner: i32) -> Result<()> {
        let (ppid, started) =
            proc_stat_ppid_and_start(pid.try_into()?).context("Creator no longer exists")?;
        ensure!(
            ppid == owner as u32 && unsafe { libc::getpgid(pid) } == pid,
            "Invalid creator group owner"
        );
        queue.watch_exit(pid)?;
        ensure!(
            start_of(pid) == Some(started),
            "Creator changed during registration"
        );
        self.0.insert(pid, started);
        Ok(())
    }

    fn stop(&mut self, pid: i32) {
        if let Some(started) = self.0.remove(&pid)
            && let Ok(info) = crate::state::darwin_bsd_info(pid, true)
            && info.pbi_pid == pid as u32
            && info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec == started
        {
            // The recorded leader's PID still exists, including a zombie held
            // by a stopped controller. Its PGID cannot have been reassigned.
            signal_group(pid, libc::SIGKILL);
        }
    }
}

impl Drop for Groups {
    fn drop(&mut self) {
        while let Some(&pid) = self.0.keys().next() {
            self.stop(pid);
        }
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
        owner > 1 && unsafe { libc::getppid() } == owner && fd >= 3,
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
    let mut groups = Groups::default();
    channel.write_all(&[ACK])?;
    loop {
        let event = queue.wait()?;
        if event.filter == libc::EVFILT_PROC {
            let pid = event.ident as i32;
            if pid == owner {
                return Ok(());
            }
            groups.stop(pid);
            continue;
        }
        let mut frame = [0; 5];
        channel.read_exact(&mut frame)?;
        let pid = i32::from_ne_bytes(frame[1..].try_into()?);
        match frame[0] {
            ARM => {
                if groups.arm(&queue, pid, owner).is_err() {
                    channel.write_all(&[0])?;
                    continue;
                }
            }
            DISARM => {
                groups.0.remove(&pid);
            }
            EXIT if pid == 0 => {
                channel.write_all(&[ACK])?;
                return Ok(());
            }
            _ => bail!("Invalid group observer message"),
        }
        channel.write_all(&[ACK])?;
    }
}
