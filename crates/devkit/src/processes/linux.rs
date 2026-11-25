//! Linux process-group supervision using subreaper and PR_SET_PDEATHSIG.
use super::*;
use std::sync::atomic::AtomicI32;

static INITIAL_PPID: AtomicI32 = AtomicI32::new(0);

pub(super) fn register_platform_signals(
    cancelled: &Arc<AtomicUsize>,
    registrations: &mut Vec<signal_hook::SigId>,
) -> Result<()> {
    let cancelled = cancelled.clone();
    // SAFETY: getppid() and lock-free atomic operations are async-signal-safe.
    // Linux PR_SET_PDEATHSIG fires when the spawning OS thread exits even while
    // the parent process (TGID) remains alive; only cancel when getppid() changes.
    let id = unsafe {
        signal_hook::low_level::register(libc::SIGURG, move || {
            let initial = INITIAL_PPID.load(Ordering::Relaxed);
            if initial > 0 && libc::getppid() != initial {
                let _ = cancelled.compare_exchange(
                    0,
                    libc::SIGTERM as usize,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
            }
        })
    }?;
    registrations.push(id);
    // Orphaned ordinary creator descendants remain our responsibility.
    // SAFETY: prctl accepts these scalar arguments, with no borrowed pointers.
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0,
        "Cannot enable child reaping"
    );
    Ok(())
}

pub(super) fn spawn(command: &mut Command, _deadline: Option<Instant>) -> Result<Creator> {
    Ok(Creator::new(command.spawn()?))
}

pub(super) fn poll_creator(child: &mut Child) -> io::Result<Option<ExitStatus>> {
    child.try_wait()
}

impl Creator {
    pub(super) fn finish_metadata(&mut self, deadline: Instant) -> Result<()> {
        // This process has no daemon-side mutation to reconcile. Stop it
        // immediately and never enter the mutation grace period on drop.
        self.finished = true;
        signal_group(self.pid, libc::SIGKILL);
        loop {
            if self.try_wait()?.is_some() {
                reap_available(self.pid)?;
                if !group_exists(self.pid) {
                    return Ok(());
                }
            }
            ensure!(
                Instant::now() < deadline,
                "Metadata query process group was not reaped"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    pub(super) fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        signal_group(self.pid, libc::SIGTERM);
        signal_group(self.pid, libc::SIGCONT);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut first_error = None;
        let mut reaped = false;
        // Reap the direct creator via Child before waitpid handles adopted
        // descendants, and still kill the group if waiting itself fails.
        while Instant::now() < deadline {
            match self.try_wait() {
                Ok(Some(_)) => {
                    reaped = true;
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(error) => {
                    first_error = Some(anyhow::Error::from(error));
                    break;
                }
            }
        }
        if !reaped {
            signal_group(self.pid, libc::SIGKILL);
        }
        if let Err(error) = self.child.wait() {
            first_error.get_or_insert(error.into());
        }
        while group_exists(self.pid) && Instant::now() < deadline {
            if let Err(error) = reap_available(self.pid) {
                first_error.get_or_insert(error);
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        if group_exists(self.pid) {
            signal_group(self.pid, libc::SIGKILL);
        }
        let reap_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Err(error) = reap_available(self.pid) {
                first_error.get_or_insert(error);
            }
            if !group_exists(self.pid) {
                self.finished = true;
                break;
            }
            if Instant::now() >= reap_deadline {
                first_error
                    .get_or_insert_with(|| anyhow::anyhow!("Creator process group was not reaped"));
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn group_exists(pid: i32) -> bool {
    // SAFETY: signal 0 only probes the creator's PGID.
    let result = unsafe { libc::kill(-pid, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn reap_available(pid: i32) -> Result<()> {
    loop {
        let mut status = 0;
        // SAFETY: waitpid is restricted to adopted children in our owned PGID.
        let waited = unsafe { libc::waitpid(-pid, &mut status, libc::WNOHANG) };
        if waited > 0 {
            continue;
        }
        if waited == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(());
        }
        return Err(error.into());
    }
}

/// The CLI dies with its invoking Make/shell; its installed handlers still get
/// to clean up. Install ProcessRunner before calling this, before any spawn.
pub fn watch_parent(runner: &ProcessRunner) -> Result<()> {
    // SAFETY: getppid and scalar prctl arguments contain no borrowed pointers.
    let parent = unsafe { libc::getppid() };
    INITIAL_PPID.store(parent, Ordering::SeqCst);
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGURG, 0, 0, 0) } == 0,
        "Cannot watch parent lifetime"
    );
    let current = unsafe { libc::getppid() };
    if current != parent || (parent != 1 && current == 1) {
        runner.cancel(libc::SIGTERM);
        runner.check_cancelled()?;
    }
    Ok(())
}

/// Restore SIGTERM parent-death notification before replacing this image via execve,
/// while guarding against a parent exit in the transition window.
pub fn restore_exec_pdeathsig(runner: &ProcessRunner) -> Result<()> {
    runner.check_cancelled()?;
    let initial = INITIAL_PPID.load(Ordering::SeqCst);
    // SAFETY: scalar prctl arguments contain no borrowed pointers.
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) } == 0,
        "Cannot restore parent death signal before exec"
    );
    let current = unsafe { libc::getppid() };
    if initial > 0 && (current != initial || (initial != 1 && current == 1)) {
        runner.cancel(libc::SIGTERM);
        runner.check_cancelled()?;
    }
    Ok(())
}
