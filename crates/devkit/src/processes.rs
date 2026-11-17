//! One process boundary for native tools and Docker creators. No shell interprets argv.
use crate::Result;
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fmt,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::process::{CommandExt, ExitStatusExt},
    },
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

static INITIAL_PPID: AtomicI32 = AtomicI32::new(0);

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
    TeeStderr(usize),
    Capture(usize),
}
pub struct ProcessOutput {
    pub code: u8,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
type MetadataRead = (Option<Box<[u8; 512]>>, Option<ExitStatus>);
pub enum CpMetadata {
    Header(Box<[u8; 512]>),
    Missing,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InspectionSnapshot {
    pub id: String,
    pub image: String,
    pub anonymous_volumes: BTreeMap<String, String>,
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
        // SAFETY: SIG_IGN is a valid POSIX disposition; inherited across execve so
        // child process groups (process_group(0)) never stall on background TTY writes.
        ensure!(
            unsafe { libc::signal(libc::SIGTTOU, libc::SIG_IGN) } != libc::SIG_ERR,
            "Cannot ignore SIGTTOU"
        );
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
        {
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
    pub(crate) fn cancel(&self, sig: i32) {
        let _ = self.signals.cancelled.compare_exchange(
            0,
            sig as usize,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
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
    /// Bound provider reads without treating an expected nonzero status (such
    /// as a registry 404) as transport success or discarding captured output.
    pub fn run_with_timeout(
        &self,
        command: &mut Command,
        mode: OutputMode,
        timeout: Duration,
    ) -> Result<ProcessOutput> {
        self.run_inner(command, mode, false, Some(timeout))
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
    pub fn capture_before(
        &self,
        command: &mut Command,
        deadline: Instant,
    ) -> Result<ProcessOutput> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .context("Metadata query deadline exceeded")?;
        self.checked_with_timeout(
            command,
            OutputMode::Capture(16 * 1024 * 1024),
            remaining.min(Duration::from_secs(3)),
        )
    }
    fn run_inner(
        &self,
        command: &mut Command,
        mode: OutputMode,
        cleanup: bool,
        timeout: Option<Duration>,
    ) -> Result<ProcessOutput> {
        self.run_input_inner(command, mode, cleanup, timeout, None, None)
    }
    pub fn run_with_input(
        &self,
        command: &mut Command,
        mode: OutputMode,
        input: &[u8],
    ) -> Result<ProcessOutput> {
        ensure!(
            input.len() <= 8 * 1024 * 1024,
            "Native stdin exceeds byte budget"
        );
        self.run_input_inner(command, mode, false, None, Some(input), None)
    }
    pub fn run_tracked(
        &self,
        command: &mut Command,
        mode: OutputMode,
        input: Option<&[u8]>,
        reaped: &mut bool,
    ) -> Result<ProcessOutput> {
        ensure!(
            input.is_none_or(|v| v.len() <= 8 * 1024 * 1024),
            "Native stdin exceeds byte budget"
        );
        *reaped = false;
        self.run_input_inner(command, mode, false, None, input, Some(reaped))
    }
    fn run_input_inner(
        &self,
        command: &mut Command,
        mode: OutputMode,
        cleanup: bool,
        timeout: Option<Duration>,
        input: Option<&[u8]>,
        mut reaped: Option<&mut bool>,
    ) -> Result<ProcessOutput> {
        if !cleanup && let Err(error) = self.check_cancelled() {
            if let Some(flag) = reaped.as_deref_mut() {
                *flag = true;
            }
            return Err(error);
        }
        command.process_group(0).stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        match mode {
            OutputMode::Inherit => {
                command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
            }
            OutputMode::Stderr => {
                command.stdout(Stdio::piped()).stderr(Stdio::inherit());
            }
            OutputMode::TeeStderr(_) => {
                command.stdout(Stdio::inherit()).stderr(Stdio::piped());
            }
            OutputMode::Capture(_) => {
                command.stdout(Stdio::piped()).stderr(Stdio::piped());
            }
        }
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                if let Some(flag) = reaped.as_deref_mut() {
                    *flag = true;
                }
                return Err(error).with_context(|| {
                    format!("Cannot start {}", command.get_program().to_string_lossy())
                });
            }
        };
        let mut creator = Creator::new(child);
        let readers_done = Arc::new(AtomicBool::new(false));
        let overflow = Arc::new(AtomicBool::new(false));
        let reader_failed = Arc::new(AtomicBool::new(false));
        let input_failed = Arc::new(AtomicBool::new(false));
        let input_writer = input.map(|bytes| {
            stdin_writer(
                creator.child.stdin.take().expect("piped input"),
                bytes.to_owned(),
                readers_done.clone(),
                input_failed.clone(),
            )
        });
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
                Ok(None) => {
                    if input_failed.load(Ordering::SeqCst) {
                        break Err(anyhow::anyhow!("Cannot write native stdin"));
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => break Err(error.into()),
            }
        };
        // This happens on every path, including wait/read errors. Do not let an
        // early ? bypass termination and reaping of the exact creator PGID.
        let stopped = creator.finish();
        if stopped.is_ok()
            && let Some(reaped) = reaped
        {
            *reaped = true;
        }
        readers_done.store(true, Ordering::SeqCst);
        // Join BOTH readers even if one fails. Their reads are nonblocking and
        // finish after the pipe is drained or a bounded post-creator interval.
        let stdout = join_reader(stdout);
        let stderr = join_reader(stderr);
        let input_result = input_writer
            .map(|writer| {
                writer
                    .join()
                    .map_err(|_| anyhow::anyhow!("Cannot join native stdin writer"))
                    .and_then(|result| result.map_err(Into::into))
            })
            .unwrap_or(Ok(()));
        let status = status?;
        let code = status_code(status);
        if code == 0 {
            input_result?;
        }
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
    /// Stop at the first exact tar header: never drain or extract image files.
    pub fn cp_metadata(&self, id: &str, path: &str, timeout: Duration) -> Result<CpMetadata> {
        ensure!(
            id.len() == 64
                && id.bytes().all(|b| b.is_ascii_hexdigit())
                && path.starts_with('/')
                && path.len() <= 4096
                && !path.contains('\0'),
            "Invalid image metadata query"
        );
        self.check_cancelled()?;
        let mut command = Command::new("docker");
        command
            .args(["cp", &format!("{id}:{path}"), "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut creator = Creator::new(
            command
                .spawn()
                .context("Cannot start image metadata query")?,
        );
        let mut stdout = creator
            .child
            .stdout
            .take()
            .context("Missing metadata stream")?;
        let done = Arc::new(AtomicBool::new(false));
        let overflow = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let stderr = creator.child.stderr.take().map(|stream| {
            reader(
                stream,
                OutputMode::Capture(8192),
                done.clone(),
                overflow.clone(),
                failed.clone(),
            )
        });
        let result = (|| -> Result<MetadataRead> {
            let fd = stdout.as_raw_fd();
            // SAFETY: this is the live, owned pipe descriptor.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            ensure!(
                flags >= 0
                    && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
                "Cannot configure metadata pipe"
            );
            let deadline = Instant::now() + timeout.min(Duration::from_secs(3));
            let mut header = Box::new([0; 512]);
            let mut count = 0;
            loop {
                self.check_cancelled()?;
                ensure!(
                    Instant::now() < deadline,
                    "Image metadata query exceeded deadline"
                );
                ensure!(
                    !overflow.load(Ordering::SeqCst) && !failed.load(Ordering::SeqCst),
                    "Image metadata stderr exceeds budget or failed"
                );
                match stdout.read(&mut header[count..]) {
                    Ok(0) => {
                        if let Some(status) = creator.child.try_wait()? {
                            ensure!(count == 0, "Truncated image metadata header");
                            return Ok((None, Some(status)));
                        }
                    }
                    Ok(read) => {
                        count += read;
                        if count == 512 {
                            return Ok((Some(header), None));
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error.into()),
                }
                thread::sleep(Duration::from_millis(2));
            }
        })();
        drop(stdout);
        let stopped = creator.finish();
        done.store(true, Ordering::SeqCst);
        let stderr = join_reader(stderr);
        stopped?;
        let stderr = stderr?;
        ensure!(
            !overflow.load(Ordering::SeqCst),
            "Image metadata stderr exceeds byte budget"
        );
        let (header, status) = result?;
        if let Some(header) = header {
            return Ok(CpMetadata::Header(header));
        }
        let expected = format!(
            "Error response from daemon: Could not find the file {path} in container {id}\n"
        );
        ensure!(
            status.is_some_and(|s| status_code(s) == 1) && stderr == expected.as_bytes(),
            "Unproved image metadata response"
        );
        Ok(CpMetadata::Missing)
    }
    /// Cleanup observations ignore cancellation but always have a deadline.
    /// Only the requested labels and namespace fields leave the Docker CLI.
    pub fn inspection_snapshot(
        &self,
        owned: &OwnedContainer,
        labels: &BTreeMap<String, String>,
        deadline: Instant,
    ) -> Result<Option<InspectionSnapshot>> {
        owned.validate()?;
        let ids = self.named_ids(owned, deadline)?;
        let Some(id) = ids.first() else {
            return Ok(None);
        };
        let mut expected = labels.clone();
        ensure!(
            expected.len() <= 4
                && expected.iter().all(|(k, v)| !k.is_empty()
                    && k.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
                    && v.len() <= 200),
            "Invalid inspection ownership labels"
        );
        if let Some(value) = expected.insert(owned.label.clone(), owned.token.clone()) {
            ensure!(value == owned.token, "Conflicting inspection token");
        }
        let mut format = "{{json .Id}}\n{{json .Name}}\n{{json .State.Status}}\n{{json .State.Pid}}\n{{json .Image}}\n{{json .HostConfig.Binds}}\n{{json .HostConfig.VolumesFrom}}\n{{json .Mounts}}".to_owned();
        for key in expected.keys() {
            format.push_str(&format!(
                "\n{{{{json (index .Config.Labels {})}}}}",
                serde_json::to_string(key)?
            ));
        }
        let output = self.query(
            Command::new("docker").args(["container", "inspect", "--format", &format, id]),
            deadline,
        )?;
        let rows = output
            .text()?
            .lines()
            .map(serde_json::from_str)
            .collect::<std::result::Result<Vec<Value>, _>>()?;
        ensure!(
            rows.len() == 8 + expected.len()
                && rows[0].as_str() == Some(id)
                && rows[1].as_str() == Some(format!("/{}", owned.name).as_str())
                && rows[2] == "created"
                && rows[3] == 0
                && (rows[5].is_null() || rows[5].as_array().is_some_and(Vec::is_empty))
                && (rows[6].is_null() || rows[6].as_array().is_some_and(Vec::is_empty)),
            "Refusing an unowned, started or user-mounted image inspector"
        );
        for (row, value) in rows[8..].iter().zip(expected.values()) {
            ensure!(
                row.as_str() == Some(value),
                "Image inspection ownership conflict"
            );
        }
        let image = rows[4]
            .as_str()
            .context("Missing inspector image")?
            .to_owned();
        ensure!(
            image
                .strip_prefix("sha256:")
                .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
            "Invalid immutable inspector image"
        );
        let mut anonymous_volumes = BTreeMap::new();
        for mount in rows[7]
            .as_array()
            .context("Invalid image inspector mounts")?
        {
            let name = mount["Name"]
                .as_str()
                .context("Missing inspection volume name")?;
            let target = mount["Destination"]
                .as_str()
                .context("Missing inspection volume destination")?;
            ensure!(
                mount["Type"] == "volume"
                    && name.len() == 64
                    && name.bytes().all(|b| b.is_ascii_hexdigit())
                    && anonymous_volumes
                        .insert(target.into(), name.into())
                        .is_none(),
                "Image inspector has an unproved mount"
            );
        }
        Ok(Some(InspectionSnapshot {
            id: id.clone(),
            image,
            anonymous_volumes,
        }))
    }
    pub fn remove_inspection(
        &self,
        owned: &OwnedContainer,
        labels: &BTreeMap<String, String>,
        recorded: &InspectionSnapshot,
        deadline: Instant,
    ) -> Result<()> {
        if let Some(actual) = self.inspection_snapshot(owned, labels, deadline)? {
            ensure!(
                actual.id == recorded.id
                    && actual.image == recorded.image
                    && actual.anonymous_volumes == recorded.anonymous_volumes,
                "Image inspector changed before cleanup"
            );
            self.run_inner(
                Command::new("docker").args(["rm", "--force", "--volumes", &actual.id]),
                OutputMode::Capture(8192),
                true,
                Some(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_secs(3)),
                ),
            )?
            .success("Image inspector cleanup")?;
        }
        ensure!(
            self.named_ids(owned, deadline)?.is_empty(),
            "Image inspector remains after cleanup"
        );
        self.verify_inspection_volumes_absent(recorded, deadline)
    }
    pub fn verify_inspection_volumes_absent(
        &self,
        recorded: &InspectionSnapshot,
        deadline: Instant,
    ) -> Result<()> {
        for name in recorded.anonymous_volumes.values() {
            ensure!(
                name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()),
                "Invalid recorded inspection volume"
            );
            let output = self.query(
                Command::new("docker").args([
                    "volume",
                    "ls",
                    "--filter",
                    &format!("name=^{name}$"),
                    "--format",
                    "{{.Name}}",
                ]),
                deadline,
            )?;
            ensure!(
                output.text()?.trim().is_empty(),
                "Image inspection volume remains; preserving its recovery intent"
            );
        }
        Ok(())
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
    pub fn run_owned_unchecked(
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
        let result = self.run_inner(command, mode, false, timeout);
        // Output/IO failures can also abort an in-flight create request. As with
        // cancellation, observe briefly AFTER its creator group has been reaped.
        let interrupted = match &result {
            Ok(o) => o.code != 0,
            Err(_) => true,
        };
        let cleanup = self.cleanup(owned, interrupted);
        match (result, cleanup) {
            (Err(primary), Err(cleanup)) => {
                eprintln!("devkit: {cleanup:#}");
                Err(primary)
            }
            (Err(primary), _) => Err(primary),
            (Ok(output), Err(cleanup)) if output.code != 0 => {
                eprintln!("devkit: {cleanup:#}");
                Ok(output)
            }
            (Ok(_), Err(error)) => Err(failure(1, format!("{error:#}"))),
            (Ok(output), Ok(())) => {
                if output.code == 0 {
                    self.check_cancelled()?;
                }
                Ok(output)
            }
        }
    }
    fn run_owned_inner(
        &self,
        command: &mut Command,
        mode: OutputMode,
        owned: &OwnedContainer,
        timeout: Option<Duration>,
    ) -> Result<ProcessOutput> {
        self.run_owned_unchecked(command, mode, owned, timeout)?
            .success("Container task")
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

fn stdin_writer(
    mut stream: std::process::ChildStdin,
    bytes: Vec<u8>,
    done: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
) -> thread::JoinHandle<io::Result<()>> {
    thread::spawn(move || {
        let result = (|| {
            let fd = stream.as_raw_fd();
            // SAFETY: the live stdin pipe belongs to this writer thread.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut offset = 0;
            while offset < bytes.len() {
                if done.load(Ordering::SeqCst) {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "Native stdin stopped with creator",
                    ));
                }
                match stream.write(&bytes[offset..(offset + 8192).min(bytes.len())]) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "Native stdin closed",
                        ));
                    }
                    Ok(count) => offset += count,
                    Err(error)
                        if [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted]
                            .contains(&error.kind()) =>
                    {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        })();
        if result.is_err() {
            failed.store(true, Ordering::SeqCst);
        }
        result
    })
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
                        OutputMode::TeeStderr(limit) => {
                            io::stderr().write_all(&block[..count])?;
                            let remaining = limit.saturating_sub(saved.len());
                            saved.extend_from_slice(&block[..count.min(remaining)]);
                        }
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
        signal_group(self.pid, libc::SIGCONT);
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
