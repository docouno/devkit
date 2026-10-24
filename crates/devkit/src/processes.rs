//! One process boundary for native tools and Docker creators. No shell interprets argv.
use crate::Result;
use anyhow::{Context, bail, ensure};
use serde_json::Value;
use std::{
    fmt,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::process::{CommandExt, ExitStatusExt},
    },
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct CommandFailure {
    pub code: u8,
    pub message: String,
}
impl fmt::Display for CommandFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for CommandFailure {}
pub fn failure(code: u8, message: impl Into<String>) -> anyhow::Error {
    CommandFailure {
        code,
        message: message.into(),
    }
    .into()
}
pub fn exit_code(error: &anyhow::Error) -> u8 {
    error
        .chain()
        .find_map(|e| e.downcast_ref::<CommandFailure>().map(|e| e.code))
        .unwrap_or(1)
}
fn status_code(status: ExitStatus) -> u8 {
    status
        .code()
        .map(|c| c.clamp(0, 255) as u8)
        .unwrap_or_else(|| (128 + status.signal().unwrap_or(1)).min(255) as u8)
}

#[derive(Clone, Copy)]
pub enum OutputMode {
    Inherit,
    Stderr,
    Capture(usize),
}
pub struct ProcessOutput {
    pub code: u8,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
impl ProcessOutput {
    pub fn success(self, label: &str) -> Result<Self> {
        if self.code != 0 {
            // Captured diagnostics must remain visible, without copying unbounded output.
            let diagnostics: Vec<_> = self
                .stderr
                .iter()
                .chain(self.stdout.iter())
                .copied()
                .take(8192)
                .collect();
            let diagnostics = String::from_utf8_lossy(&diagnostics);
            return Err(failure(
                self.code,
                format!(
                    "{label} failed (exit {}): {}",
                    self.code,
                    diagnostics.trim()
                ),
            ));
        }
        Ok(self)
    }
    pub fn text(&self) -> Result<&str> {
        Ok(std::str::from_utf8(&self.stdout)?)
    }
}
struct Signals {
    cancelled: Arc<AtomicUsize>,
    registrations: Vec<signal_hook::SigId>,
}
impl Drop for Signals {
    fn drop(&mut self) {
        for id in &self.registrations {
            signal_hook::low_level::unregister(*id);
        }
    }
}
#[derive(Clone)]
pub struct ProcessRunner {
    signals: Arc<Signals>,
}
#[derive(Clone, Debug)]
pub struct OwnedContainer {
    pub name: String,
    pub token: String,
    pub label: String,
}
impl OwnedContainer {
    pub fn new(name: String, token: String) -> Self {
        Self {
            name,
            token,
            label: "io.docouno.devkit.invocation".into(),
        }
    }
    fn validate(&self) -> Result<()> {
        for value in [&self.name, &self.token] {
            ensure!(
                !value.is_empty()
                    && value.len() < 200
                    && value.as_bytes()[0].is_ascii_alphanumeric()
                    && value
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
                "Invalid container ownership identity"
            );
        }
        ensure!(
            !self.label.is_empty()
                && self
                    .label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._/-".contains(&c)),
            "Invalid ownership label"
        );
        Ok(())
    }
}
impl ProcessRunner {
    pub fn new() -> Result<Self> {
        let cancelled = Arc::new(AtomicUsize::new(0));
        let mut signals = Signals {
            cancelled: cancelled.clone(),
            registrations: Vec::new(),
        };
        for sig in [libc::SIGINT, libc::SIGTERM] {
            let cancelled = cancelled.clone();
            // SAFETY: the handler only touches a lock-free atomic; it does not allocate or perform I/O.
            let id = unsafe {
                signal_hook::low_level::register(sig, move || {
                    let _ = cancelled.compare_exchange(
                        0,
                        sig as usize,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    );
                })
            }?;
            signals.registrations.push(id);
        }
        // Orphaned ordinary creator descendants remain our responsibility.
        // SAFETY: prctl accepts these scalar arguments, with no borrowed pointers.
        ensure!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0,
            "Cannot enable child reaping"
        );
        Ok(Self {
            signals: Arc::new(signals),
        })
    }
    pub fn check_cancelled(&self) -> Result<()> {
        let sig = self.signals.cancelled.load(Ordering::SeqCst);
        if sig != 0 {
            return Err(failure(
                (128 + sig) as u8,
                format!("Cancelled by signal {sig}"),
            ));
        }
        Ok(())
    }
    pub fn run(&self, command: &mut Command, mode: OutputMode) -> Result<ProcessOutput> {
        self.run_inner(command, mode, false, None)
    }
    pub fn checked(&self, command: &mut Command, mode: OutputMode) -> Result<ProcessOutput> {
        self.run(command, mode)?
            .success(&command.get_program().to_string_lossy())
    }
    pub fn checked_with_timeout(
        &self,
        command: &mut Command,
        mode: OutputMode,
        timeout: Duration,
    ) -> Result<ProcessOutput> {
        self.run_inner(command, mode, false, Some(timeout))?
            .success(&command.get_program().to_string_lossy())
    }
    pub fn capture(&self, command: &mut Command) -> Result<ProcessOutput> {
        self.checked(command, OutputMode::Capture(16 * 1024 * 1024))
    }
    fn run_inner(
        &self,
        command: &mut Command,
        mode: OutputMode,
        cleanup: bool,
        timeout: Option<Duration>,
    ) -> Result<ProcessOutput> {
        if !cleanup {
            self.check_cancelled()?;
        }
        command.process_group(0).stdin(Stdio::null());
        match mode {
            OutputMode::Inherit => {
                command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
            }
            OutputMode::Stderr => {
                command.stdout(Stdio::piped()).stderr(Stdio::inherit());
            }
            OutputMode::Capture(_) => {
                command.stdout(Stdio::piped()).stderr(Stdio::piped());
            }
        }
        let child = command
            .spawn()
            .with_context(|| format!("Cannot start {}", command.get_program().to_string_lossy()))?;
        let mut creator = Creator::new(child);
        let readers_done = Arc::new(AtomicBool::new(false));
        let overflow = Arc::new(AtomicBool::new(false));
        let reader_failed = Arc::new(AtomicBool::new(false));
        let stdout = creator.child.stdout.take().map(|stream| {
            reader(
                stream,
                mode,
                readers_done.clone(),
                overflow.clone(),
                reader_failed.clone(),
            )
        });
        let stderr = creator.child.stderr.take().map(|stream| {
            reader(
                stream,
                mode,
                readers_done.clone(),
                overflow.clone(),
                reader_failed.clone(),
            )
        });
        let started = Instant::now();
        let status = loop {
            if !cleanup && let Err(error) = self.check_cancelled() {
                break Err(error);
            }
            if timeout.is_some_and(|t| started.elapsed() >= t) {
                break Err(anyhow::anyhow!("Process exceeded its deadline"));
            }
            if overflow.load(Ordering::SeqCst) {
                break Err(anyhow::anyhow!("Native command output exceeds byte budget"));
            }
            if reader_failed.load(Ordering::SeqCst) {
                break Err(anyhow::anyhow!("Cannot read native command output"));
            }
            match creator.child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(error) => break Err(error.into()),
            }
        };
        // This happens on every path, including wait/read errors. Do not let an
        // early ? bypass termination and reaping of the exact creator PGID.
        let stopped = creator.finish();
        readers_done.store(true, Ordering::SeqCst);
        // Join BOTH readers even if one fails. Their reads are nonblocking and
        // finish after the pipe is drained or a bounded post-creator interval.
        let stdout = join_reader(stdout);
        let stderr = join_reader(stderr);
        let status = status?;
        let code = status_code(status);
        for result in [
            stopped,
            stdout
                .as_ref()
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!("{e:#}")),
            stderr
                .as_ref()
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!("{e:#}")),
        ] {
            if let Err(error) = result {
                if code == 0 {
                    return Err(error);
                }
                eprintln!("devkit: {error:#}");
            }
        }
        let output = ProcessOutput {
            code,
            stdout: stdout.unwrap_or_default(),
            stderr: stderr.unwrap_or_default(),
        };
        if code == 0 {
            if !cleanup {
                self.check_cancelled()?;
            }
            ensure!(
                !overflow.load(Ordering::SeqCst),
                "Native command output exceeds byte budget"
            );
        }
        Ok(output)
    }
    fn query(&self, command: &mut Command, deadline: Instant) -> Result<ProcessOutput> {
        let timeout = deadline
            .checked_duration_since(Instant::now())
            .context("Container cleanup deadline exceeded")?;
        self.run_inner(
            command,
            OutputMode::Capture(1024 * 1024),
            true,
            Some(timeout.min(Duration::from_secs(3))),
        )?
        .success("Docker ownership query")
    }
    fn named_ids(&self, owned: &OwnedContainer, deadline: Instant) -> Result<Vec<String>> {
        let output = self
            .query(
                Command::new("docker").args([
                    "container",
                    "ls",
                    "--all",
                    "--no-trunc",
                    "--filter",
                    &format!("name=^/{}$", owned.name),
                    "--format",
                    "{{.ID}}",
                ]),
                deadline,
            )
            .context("Cannot verify cleanup/container ownership")?;
        let ids: Vec<_> = output
            .text()?
            .lines()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        ensure!(
            ids.len() <= 1
                && ids
                    .iter()
                    .all(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())),
            "Invalid exact-name Docker inventory"
        );
        Ok(ids)
    }
    pub fn run_owned(
        &self,
        command: &mut Command,
        mode: OutputMode,
        owned: &OwnedContainer,
    ) -> Result<ProcessOutput> {
        self.run_owned_inner(command, mode, owned, None)
    }
    pub fn run_owned_with_timeout(
        &self,
        command: &mut Command,
        mode: OutputMode,
        owned: &OwnedContainer,
        timeout: Duration,
    ) -> Result<ProcessOutput> {
        self.run_owned_inner(command, mode, owned, Some(timeout))
    }
    fn run_owned_inner(
        &self,
        command: &mut Command,
        mode: OutputMode,
        owned: &OwnedContainer,
        timeout: Option<Duration>,
    ) -> Result<ProcessOutput> {
        owned.validate()?;
        self.check_cancelled()?;
        ensure!(
            self.named_ids(owned, Instant::now() + Duration::from_secs(8))?
                .is_empty(),
            "Container name already exists: {}",
            owned.name
        );
        let result = self
            .run_inner(command, mode, false, timeout)
            .and_then(|o| o.success("Container task"));
        // Output/IO failures can also abort an in-flight create request. As with
        // cancellation, observe briefly AFTER its creator group has been reaped.
        let interrupted = result.is_err();
        let cleanup = self.cleanup(owned, interrupted);
        match (result, cleanup) {
            (Err(primary), Err(cleanup)) => {
                eprintln!("devkit: {cleanup:#}");
                Err(primary)
            }
            (Err(primary), _) => Err(primary),
            (Ok(_), Err(error)) => Err(failure(1, format!("{error:#}"))),
            (Ok(output), Ok(())) => {
                self.check_cancelled()?;
                Ok(output)
            }
        }
    }
    fn cleanup(&self, owned: &OwnedContainer, interrupted: bool) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(8);
        let attempts = if interrupted { 10 } else { 1 };
        for attempt in 0..attempts {
            for id in self.named_ids(owned, deadline)? {
                let inspected = self.query(
                    Command::new("docker").args([
                        "container",
                        "inspect",
                        "--format",
                        "{{json .}}",
                        &id,
                    ]),
                    deadline,
                );
                let inspected = match inspected {
                    Ok(v) => v,
                    Err(e) => {
                        if self.named_ids(owned, deadline)?.is_empty() {
                            continue;
                        }
                        return Err(e);
                    }
                };
                let object: Value = serde_json::from_slice(&inspected.stdout)?;
                ensure!(
                    object["Id"].as_str() == Some(id.as_str())
                        && object["Name"].as_str() == Some(format!("/{}", owned.name).as_str())
                        && object["Config"]["Labels"][&owned.label].as_str()
                            == Some(owned.token.as_str()),
                    "Refusing unowned container {}",
                    owned.name
                );
                // Immutable ID, never a name which could be reassigned between calls.
                let remaining = deadline.saturating_duration_since(Instant::now());
                let _ = self.run_inner(
                    Command::new("docker").args(["stop", "--time", "2", &id]),
                    OutputMode::Capture(1024 * 1024),
                    true,
                    Some(remaining.min(Duration::from_secs(3))),
                );
                let remaining = deadline.saturating_duration_since(Instant::now());
                let _ = self.run_inner(
                    Command::new("docker").args(["rm", "--force", &id]),
                    OutputMode::Capture(1024 * 1024),
                    true,
                    Some(remaining.min(Duration::from_secs(2))),
                );
            }
            if attempt + 1 < attempts {
                thread::sleep(Duration::from_millis(100));
            }
        }
        ensure!(
            self.named_ids(owned, deadline)?.is_empty(),
            "Cleanup incomplete for {}",
            owned.name
        );
        Ok(())
    }
}

type Reader = thread::JoinHandle<io::Result<Vec<u8>>>;
fn reader(
    mut stream: impl Read + AsRawFd + Send + 'static,
    mode: OutputMode,
    done: Arc<AtomicBool>,
    overflow: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
) -> Reader {
    thread::spawn(move || {
        let result = (|| {
            let fd = stream.as_raw_fd();
            // SAFETY: the owned pipe descriptor is valid for this reader's lifetime.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut saved = Vec::new();
            let mut block = [0; 8192];
            let mut finished_at = None;
            loop {
                if done.load(Ordering::SeqCst) {
                    let at = finished_at.get_or_insert_with(Instant::now);
                    if at.elapsed() >= Duration::from_millis(100) {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "Output pipe did not drain after creator exit",
                        ));
                    }
                }
                match stream.read(&mut block) {
                    Ok(0) => break,
                    Ok(count) => match mode {
                        OutputMode::Stderr => io::stderr().write_all(&block[..count])?,
                        OutputMode::Capture(limit) => {
                            let remaining = limit.saturating_sub(saved.len());
                            saved.extend_from_slice(&block[..count.min(remaining)]);
                            if count > remaining {
                                overflow.store(true, Ordering::SeqCst);
                            }
                        }
                        OutputMode::Inherit => {}
                    },
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if done.load(Ordering::SeqCst) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(saved)
        })();
        if result.is_err() {
            failed.store(true, Ordering::SeqCst);
        }
        result
    })
}
fn join_reader(handle: Option<Reader>) -> Result<Vec<u8>> {
    match handle {
        Some(handle) => handle
            .join()
            .map_err(|_| anyhow::anyhow!("Output reader panicked"))?
            .map_err(Into::into),
        None => Ok(Vec::new()),
    }
}

/// Own the creator from spawn through every error path. Never wait/reap an
/// unrelated process or rely on Drop of std::process::Child to terminate it.
struct Creator {
    child: Child,
    pid: i32,
    finished: bool,
}
impl Creator {
    fn new(child: Child) -> Self {
        let pid = child.id() as i32;
        Self {
            child,
            pid,
            finished: false,
        }
    }
    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        signal_group(self.pid, libc::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut first_error = None;
        let mut reaped = false;
        // Reap the direct creator via Child before waitpid handles adopted
        // descendants, and still kill the group if waiting itself fails.
        while Instant::now() < deadline {
            match self.child.try_wait() {
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
impl Drop for Creator {
    fn drop(&mut self) {
        if !self.finished
            && let Err(error) = self.finish()
        {
            eprintln!("devkit: Cannot reap creator: {error:#}");
        }
    }
}
fn signal_group(pid: i32, signal: i32) {
    // SAFETY: a negative PID addresses only the creator's own process group.
    unsafe {
        libc::kill(-pid, signal);
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
pub fn watch_parent() -> Result<()> {
    // SAFETY: these prctl arguments contain no borrowed pointers.
    let parent = unsafe { libc::getppid() };
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) } == 0,
        "Cannot watch parent lifetime"
    );
    if unsafe { libc::getppid() } != parent {
        bail!("Invoking process exited during startup");
    }
    Ok(())
}
