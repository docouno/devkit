//! Darwin has no parent-death signal or child subreaper. Watch the invoking
//! parent in-process via kqueue and keep each creator child un-reaped until
//! its process group drains.
use super::*;
use crate::state::proc_stat_ppid_and_start;
use std::{
    ffi::OsStr,
    os::fd::{FromRawFd, OwnedFd},
    ptr,
    sync::OnceLock,
};

pub const PARENT_HANDOFF: &str = "DEVKIT_EXEC_PARENT";
static PARENT: OnceLock<(i32, u64)> = OnceLock::new();
const BACKGROUND_DRAIN_BUDGET: Duration = Duration::from_secs(5);

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

    fn watch_exit(&self, pid: i32) -> io::Result<()> {
        let event = libc::kevent {
            ident: pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
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

pub(super) fn register_platform_signals(
    _cancelled: &Arc<AtomicUsize>,
    _registrations: &mut Vec<signal_hook::SigId>,
) -> Result<()> {
    Ok(())
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

pub(super) fn set_handoff(runner: &ProcessRunner, command: &mut Command) -> Result<()> {
    if let Some((pid, started)) = PARENT.get() {
        if unsafe { libc::getppid() } != *pid || (*started != 0 && start_of(*pid) != Some(*started))
        {
            runner.cancel(libc::SIGTERM);
            return runner.check_cancelled();
        }
        command.env(PARENT_HANDOFF, format!("{pid}:{started}"));
    }
    Ok(())
}

pub fn restore_exec_pdeathsig(runner: &ProcessRunner) -> Result<()> {
    runner.check_cancelled()
}

pub(super) fn spawn(command: &mut Command, _deadline: Option<Instant>) -> Result<Creator> {
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
    Ok(Creator::new(command.spawn()?))
}

/// Observe status without collecting the direct child so its PID continues to
/// pin the process group's numeric identity until cleanup finishes.
pub(super) fn poll_creator(child: &mut Child) -> io::Result<Option<ExitStatus>> {
    loop {
        // Darwin can leave siginfo untouched when no child event is ready.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WSTOPPED | libc::WCONTINUED | libc::WNOHANG | libc::WNOWAIT,
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
                libc::CLD_STOPPED | libc::CLD_TRAPPED | libc::CLD_CONTINUED => return Ok(None),
                code => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "Unexpected waitid event for child {}: si_pid={}, si_code={code}, si_status={}",
                            child.id(),
                            info.si_pid,
                            info.si_status
                        ),
                    ));
                }
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

fn drain_until(pgid: i32, deadline: Instant) -> bool {
    // killpg targets a snapshot. Keep the leader un-reaped to pin the PGID
    // and repeat until a fresh snapshot proves no ordinary descendant remains
    // or the finite deadline expires.
    let mut delay = Duration::from_millis(2);
    loop {
        signal_group(pgid, libc::SIGKILL);
        if matches!(drained(pgid), Ok(true)) {
            return true;
        }
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(delay);
        if wait.is_zero() {
            return false;
        }
        thread::sleep(wait);
        delay = (delay * 2).min(Duration::from_millis(100));
    }
}

fn reap(pid: i32) {
    while unsafe { libc::waitpid(pid, ptr::null_mut(), 0) } < 0
        && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
    {}
}

// Transfer a pending wait past its caller's deadline with a bounded drain budget
// so an unkillable or cross-UID member never wedges cleanup forever.
fn reap_later(pid: i32, group: Option<i32>) {
    let cleanup = move || {
        if let Some(pgid) = group {
            drain_until(pgid, Instant::now() + BACKGROUND_DRAIN_BUDGET);
        }
        reap(pid);
    };
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
        let _ = self.child.kill();
        self.finished = true;
        if !drain_until(self.pid, deadline) {
            reap_later(self.child.id() as i32, Some(self.pid));
            anyhow::bail!("Creator process group cleanup exceeded its deadline");
        }
        wait_before(&mut self.child, deadline)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observing_normal_exit_keeps_the_leader_waitable_until_cleanup() {
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
            poll_creator(&mut creator.child).unwrap().unwrap().code(),
            Some(37)
        );
        creator.finish().unwrap();
        assert_eq!(
            poll_creator(&mut creator.child).unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
        creator.finish().unwrap();
    }
}
