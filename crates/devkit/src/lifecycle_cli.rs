//! Argument completion and the native lifecycle entry points.
use crate::{
    Result,
    cache::ReleaseCache,
    catalog,
    cli::{Globals, Options, error},
    compose::admit_compose_inventory,
    files::*,
    model::*,
    processes::ProcessRunner,
    project::{self, ComponentSpec, DockerNpmResolver, InitOptions, UpdateOptions},
};
use anyhow::{Context, ensure};
use serde_json::json;
use std::{
    fs::{File, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    path::PathBuf,
};

struct Cancellation<'a>(&'a ProcessRunner);
impl crate::transaction::MutationHooks for Cancellation<'_> {
    fn after_stage(&self) -> Result<()> {
        self.0.check_cancelled()
    }
    fn after_write(&self, _count: usize, _path: &str) -> Result<()> {
        self.0.check_cancelled()
    }
}

pub fn run(command: &str, globals: &Globals, runner: &ProcessRunner) -> Result<()> {
    let args = if globals.args.is_empty() {
        &[][..]
    } else {
        &globals.args[1..]
    };
    let allowed: &[&str] = match command {
        "init" => &[
            "--app",
            "--preset",
            "--template",
            "--name",
            "--package-source",
            "--to",
            "--release-root",
        ],
        "add-app" => &["--name", "--template", "--use", "--release-root"],
        "add-package" => &["--name", "--template", "--release-root"],
        "register" => &["--app", "--package", "--profile", "--release-root"],
        "update" => &["--to", "--preview", "--package-source", "--release-root"],
        "recover" => &[],
        _ => unreachable!(),
    };
    let mut options = Options::parse(
        args,
        allowed,
        if command == "init" { &["--app"] } else { &[] },
        &["--preview"],
    )?;
    let missing = match command {
        "init" => {
            options.target.is_none()
                || !(options.flag("--app")
                    || options.flag("--preset")
                    || options.flag("--template"))
        }
        "add-app" | "add-package" => !options.flag("--name") || !options.flag("--template"),
        "register" => {
            !(options.flag("--app") || options.flag("--package")) || !options.flag("--profile")
        }
        "update" => !options.flag("--to"),
        "recover" => false,
        _ => unreachable!(),
    };
    if missing {
        let ci = std::env::var("CI").is_ok_and(|s| {
            !s.is_empty() && !["0", "false"].contains(&s.to_ascii_lowercase().as_str())
        });
        let terminal = io::stdin().is_terminal()
            || globals.interactive
            || (std::env::var_os("DEVKIT_BOOTSTRAP_INTERACTIVE").is_some_and(|v| v == "1")
                && io::stderr().is_terminal());
        if globals.non_interactive || ci || !terminal {
            return Err(error(
                "MISSING_ARGUMENT",
                format!(
                    "{command} requires complete arguments in non-interactive mode; see devkit --help"
                ),
            ));
        }
        complete(command, &mut options, &mut Prompt::open(runner)?)?;
    }
    let root = project_root(&options.target.clone().unwrap_or(std::env::current_dir()?))?;
    // Recovery is intentionally before manifest/release/cache admission. A valid
    // journal can restore a missing or partially restored accepted manifest.
    if command == "recover" {
        let recovered = project::recover_project(&root)?;
        runner.check_cancelled()?;
        globals.emit(
            command,
            json!({"root":root,"recovered":recovered}),
            if recovered {
                "Rolled back the pending operation."
            } else {
                "No pending operation."
            },
        );
        return Ok(());
    }
    let mode = options
        .value("--package-source")?
        .map(|value| match value {
            "local" => Ok(PackageMode::Local),
            "registry" => Ok(PackageMode::Registry),
            _ => Err(error(
                "INVALID_ARGUMENT",
                "Package source must be local or registry",
            )),
        })
        .transpose()?;
    let cache = ReleaseCache::new(globals.cache_dir.as_deref(), runner)?;
    let cancellation = Cancellation(runner);
    let source = options.path("--release-root");
    if command == "init" {
        let version = options.value("--to")?.unwrap_or(env!("CARGO_PKG_VERSION"));
        let apps = options.all("--app")?;
        let apps = if apps.is_empty() {
            None
        } else {
            Some(
                apps.iter()
                    .map(|entry| {
                        let (name, template) = entry
                            .split_once('=')
                            .context("Expected --app NAME=TEMPLATE")?;
                        slug(name)?;
                        catalog::template(template)?;
                        Ok(ComponentSpec {
                            name: name.to_owned(),
                            template: template.to_owned(),
                            uses: Vec::new(),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            )
        };
        let release = cache.acquire(version, None, source.as_deref(), globals.offline)?;
        let state = project::init_project(
            &release.root,
            &root,
            &InitOptions {
                template: options.value("--template")?.map(str::to_owned),
                apps,
                preset: options.value("--preset")?.map(str::to_owned),
                package_mode: mode,
                name: options.value("--name")?.map(str::to_owned),
                hooks: Some(&cancellation),
                ..Default::default()
            },
            &DockerNpmResolver,
        )?;
        runner.check_cancelled()?;
        globals.emit(command, json!({"root":root,"version":state.version,"releaseIdentity":state.release_identity,"generation":state.generation,"components":state.components.keys().collect::<Vec<_>>()}),
            &format!("Created {} with Devkit {}. Run make setup.", root.display(), state.version));
        return Ok(());
    }
    let old = crate::state::read_project_state(&root)?;
    let version = if command == "update" {
        options.required("--to")?
    } else {
        &old.version
    };
    let expected = if command == "update" {
        None
    } else {
        Some(old.release_identity.as_str())
    };
    let release = cache.acquire(version, expected, source.as_deref(), globals.offline)?;
    let inventory = if command == "add-app" {
        Some(admit_compose_inventory(
            &root,
            &crate::runtime::DockerComposeNormalizer {
                runner: runner.clone(),
            },
        )?)
    } else {
        None
    };
    let update = UpdateOptions {
        package_mode: mode,
        compose_inventory: inventory.as_ref(),
        preview: options.flag("--preview"),
        hooks: Some(&cancellation),
        ..Default::default()
    };
    let outcome = match command {
        "update" => project::update_project(&release.root, &root, &update, &DockerNpmResolver)?,
        "register" => {
            ensure!(
                options.flag("--app") != options.flag("--package"),
                "Register requires exactly one --app or --package"
            );
            let app = options.flag("--app");
            let name = options.required(if app { "--app" } else { "--package" })?;
            slug(name)?;
            let profile: ToolingProfile =
                serde_json::from_value(json!(options.required("--profile")?))
                    .context("Unsupported tooling profile")?;
            project::register_project_component(
                &release.root,
                &root,
                if app { Role::App } else { Role::Library },
                name.to_owned(),
                profile,
                &update,
                &DockerNpmResolver,
            )?
        }
        "add-app" | "add-package" => {
            let name = options.required("--name")?;
            slug(name)?;
            let template = options.required("--template")?;
            catalog::template(template)?;
            let uses = options
                .value("--use")?
                .map(|v| {
                    slug(v)?;
                    Ok::<_, anyhow::Error>(vec![v.to_owned()])
                })
                .transpose()?
                .unwrap_or_default();
            project::add_project_component(
                &release.root,
                &root,
                if command == "add-app" {
                    Role::App
                } else {
                    Role::Library
                },
                ComponentSpec {
                    name: name.to_owned(),
                    template: template.to_owned(),
                    uses,
                },
                &update,
                &DockerNpmResolver,
            )?
        }
        _ => unreachable!(),
    };
    runner.check_cancelled()?;
    if options.flag("--preview") && !globals.json {
        for file in &outcome.files {
            println!(
                "{} {}",
                if file.before.is_none() {
                    "A"
                } else if file.after.is_none() {
                    "D"
                } else {
                    "M"
                },
                file.path
            );
        }
    }
    let message = if !outcome.applied && !outcome.no_op {
        format!(
            "Preview for Devkit {}: {} changed files. No update applied.",
            outcome.state.version,
            outcome.files.len()
        )
    } else if outcome.no_op {
        format!(
            "Devkit {} is already accepted; no changes.",
            outcome.state.version
        )
    } else {
        format!(
            "Accepted Devkit {}. Review git diff, then run make setup.",
            outcome.state.version
        )
    };
    globals.emit(command, json!({"root":root,"version":outcome.state.version,"releaseIdentity":outcome.state.release_identity,"generation":outcome.state.generation,"applied":outcome.applied,"noOp":outcome.no_op,"files":outcome.files}), &message);
    Ok(())
}

fn complete(command: &str, options: &mut Options, prompt: &mut Prompt<'_>) -> Result<()> {
    if command == "init" {
        if options.target.is_none() {
            options.target = Some(PathBuf::from(
                prompt.ask("Project directory", Some("my-project"))?,
            ));
        }
        if !(options.flag("--app") || options.flag("--preset") || options.flag("--template")) {
            loop {
                let template = prompt.choose(
                    "Application template",
                    &[
                        "Node API",
                        "Rust API",
                        "React web",
                        "Electron React",
                        "Full stack: Rust API + shared React web and Electron",
                    ],
                )?;
                if template == 4 {
                    ensure!(
                        !options.flag("--app"),
                        "The full-stack preset is selected on its own"
                    );
                    options.set("--preset", "fullstack-desktop");
                    break;
                }
                let name = prompt.ask(
                    "Application name",
                    Some(["api", "api", "web", "desktop"][template]),
                )?;
                slug(&name)?;
                options.add(
                    "--app",
                    format!(
                        "{name}={}",
                        ["node-api", "rust-api", "react-web", "electron-react"][template]
                    ),
                );
                if !["y", "yes"].contains(
                    &prompt
                        .ask("Add another application? y/N", Some("n"))?
                        .to_ascii_lowercase()
                        .as_str(),
                ) {
                    break;
                }
            }
        }
        if !options.flag("--name")
            && options
                .target
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|p| p.to_str())
                .is_none_or(|name| slug(name).is_err())
        {
            let name = prompt.ask(
                "Project name (lower-case letters, digits and hyphens)",
                Some("project"),
            )?;
            slug(&name)?;
            options.set("--name", name);
        }
    } else if ["add-app", "add-package"].contains(&command) {
        if !options.flag("--name") {
            let name = prompt.ask(
                if command == "add-app" {
                    "Application name"
                } else {
                    "Package name"
                },
                None,
            )?;
            slug(&name)?;
            options.set("--name", name);
        }
        if !options.flag("--template") {
            if command == "add-package" {
                eprintln!("Package template: react-client");
                options.set("--template", "react-client");
            } else {
                let selected = prompt.choose(
                    "Application template",
                    &["Node API", "Rust API", "React web", "Electron React"],
                )?;
                options.set(
                    "--template",
                    ["node-api", "rust-api", "react-web", "electron-react"][selected],
                );
            }
        }
    } else if command == "register" {
        if !(options.flag("--app") || options.flag("--package")) {
            let app = prompt.choose("Existing component", &["Application", "Shared package"])? == 0;
            let name = prompt.ask("Existing component name", None)?;
            slug(&name)?;
            options.set(if app { "--app" } else { "--package" }, name);
        }
        if !options.flag("--profile") {
            let profiles: &[&str] = if options.flag("--app") {
                &["node", "web", "electron", "rust"]
            } else {
                &["react-library", "rust"]
            };
            let selected = prompt.choose("Tooling profile", profiles)?;
            options.set("--profile", profiles[selected]);
        }
    } else if command == "update" && !options.flag("--to") {
        let version = prompt.ask(
            "Exact Devkit version to accept",
            Some(env!("CARGO_PKG_VERSION")),
        )?;
        crate::files::version(&version)?;
        options.set("--to", version);
    }
    Ok(())
}

struct Prompt<'a> {
    input: File,
    output: Option<File>,
    runner: &'a ProcessRunner,
}
impl<'a> Prompt<'a> {
    fn open(runner: &'a ProcessRunner) -> Result<Self> {
        if io::stdin().is_terminal() {
            // SAFETY: dup creates an independently owned descriptor, not stdin.
            let fd = unsafe { libc::dup(io::stdin().as_raw_fd()) };
            ensure!(fd >= 0, "Cannot open terminal input");
            Ok(Self {
                input: unsafe { File::from_raw_fd(fd) },
                output: None,
                runner,
            })
        } else {
            let tty = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .context("Interactive input requires a controlling terminal")?;
            Ok(Self {
                input: tty.try_clone()?,
                output: Some(tty),
                runner,
            })
        }
    }
    fn write(&mut self, message: &str) -> Result<()> {
        if let Some(file) = &mut self.output {
            file.write_all(message.as_bytes())?;
            file.flush()?;
        } else {
            eprint!("{message}");
            io::stderr().flush()?;
        }
        Ok(())
    }
    fn ask(&mut self, label: &str, default: Option<&str>) -> Result<String> {
        self.write(&format!(
            "{label}{}: ",
            default.map(|v| format!(" [{v}]")).unwrap_or_default()
        ))?;
        let mut answer = Vec::new();
        loop {
            self.runner.check_cancelled()?;
            let mut poll = libc::pollfd {
                fd: self.input.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one live pollfd; the timeout lets signals cancel prompts.
            let ready = unsafe { libc::poll(&mut poll, 1, 100) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if ready == 0 {
                continue;
            }
            let mut byte = [0_u8; 1];
            match self.input.read(&mut byte) {
                Ok(0) => {
                    return Err(error(
                        "INPUT_CLOSED",
                        "Terminal input was closed; no operation started",
                    ));
                }
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => {
                    ensure!(
                        answer.len() < 4096,
                        "Terminal answer exceeds its byte budget"
                    );
                    answer.push(byte[0]);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        let answer = String::from_utf8(answer)?.trim().to_owned();
        if answer.is_empty() {
            default
                .map(str::to_owned)
                .ok_or_else(|| error("MISSING_ARGUMENT", "A nonempty answer is required"))
        } else {
            Ok(answer)
        }
    }
    fn choose(&mut self, label: &str, choices: &[&str]) -> Result<usize> {
        for (index, value) in choices.iter().enumerate() {
            self.write(&format!("  {}. {value}\n", index + 1))?;
        }
        loop {
            let answer = self.ask(label, Some("1"))?;
            if let Ok(index) = answer.parse::<usize>()
                && index > 0
                && index <= choices.len()
            {
                return Ok(index - 1);
            }
            self.write("Choose one of the listed numbers.\n")?;
        }
    }
}
