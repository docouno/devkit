use crate::{
    Result,
    processes::{self, OutputMode, ProcessRunner},
    runtime::{self, ExecRequest, RuntimeRequest},
};
use anyhow::{Context, bail, ensure};
use std::{
    ffi::{OsStr, OsString},
    fs,
    path::PathBuf,
    process::Command,
};
const HELP: &str = "Usage:\n  devkit run COMMAND [--root PATH] [--app NAME | --package NAME]\n  devkit exec --component apps/NAME [--root PATH] [--write] [--network none|bridge] [--prepare] -- ARGV\n  devkit task COMMAND [--root PATH] [--component apps/NAME]\n  devkit describe [--root PATH]\n  devkit catalog\n  devkit --version\n\nRun delegates the real root Make DAG. Make exec command= uses shellwords quoting\nwithout variable/command expansion; native exec -- ARGV preserves exact arguments.\n";
fn string(arg: &OsStr) -> Result<&str> {
    arg.to_str().context("Expected UTF-8 command option")
}
fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}
fn boolean(value: &str, label: &str) -> Result<bool> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => bail!("{label} must be 0 or 1"),
    }
}
pub fn main(args: Vec<OsString>, runner: &ProcessRunner) -> Result<()> {
    let Some(first) = args.first() else {
        print!("{HELP}");
        return Ok(());
    };
    let command = string(first)?;
    if ["--version", "version"].contains(&command) {
        ensure!(args.len() == 1, "version takes no arguments");
        println!("devkit {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if ["--help", "help", "-h"].contains(&command) {
        print!("{HELP}");
        return Ok(());
    }
    if command == "catalog" {
        ensure!(args.len() == 1, "catalog takes no arguments");
        println!("node-api\nrust-api\nreact-web\nelectron-react\nreact-client");
        return Ok(());
    }
    if command == "internal" {
        return internal(&args[1..], runner);
    }
    ensure!(
        ["task", "run", "exec", "describe"].contains(&command),
        "Unknown command: {command}"
    );
    let mut index = 1;
    let task = if ["task", "run"].contains(&command) {
        let t = args.get(index).context("Expected task command")?;
        index += 1;
        string(t)?.to_owned()
    } else {
        command.to_owned()
    };
    let mut request = RuntimeRequest::new(std::env::current_dir()?, task);
    if command == "task" || command == "exec" {
        request.component = env_string("DEVKIT_TARGET_COMPONENT");
        request.app = env_string("DEVKIT_APP");
        request.package = env_string("DEVKIT_PACKAGE");
        request.expected_manifest = env_string("DEVKIT_EXPECTED_MANIFEST");
        request.headless = boolean(
            &env_string("DEVKIT_HEADLESS").unwrap_or_else(|| "0".into()),
            "headless",
        )?;
        request.follow = boolean(
            &env_string("DEVKIT_FOLLOW").unwrap_or_else(|| "0".into()),
            "follow",
        )?;
        request.tail = env_string("DEVKIT_TAIL")
            .unwrap_or_else(|| "100".into())
            .parse()
            .context("Invalid logs tail")?;
    }
    let mut write = false;
    let mut network = "none".to_owned();
    let mut prepare = false;
    let mut argv = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    while index < args.len() {
        let key = string(&args[index])?;
        index += 1;
        if key == "--" {
            argv = args[index..].to_vec();
            break;
        }
        ensure!(seen.insert(key.to_owned()), "Repeated option: {key}");
        let permitted = match key {
            "--root" => true,
            "--component" | "--write" | "--prepare" | "--network" => {
                command == "exec" || key == "--component" && command == "task"
            }
            "--expected-manifest" => command == "task",
            "--app" | "--package" => ["task", "run", "exec"].contains(&command),
            "--headless" | "--follow" | "--tail" => ["task", "run"].contains(&command),
            _ => false,
        };
        ensure!(permitted, "Option {key} is not valid for {command}");
        match key {
            "--write" if command == "exec" => write = true,
            "--prepare" if command == "exec" => prepare = true,
            "--root"
            | "--component"
            | "--app"
            | "--package"
            | "--expected-manifest"
            | "--headless"
            | "--follow"
            | "--tail"
            | "--network" => {
                let value = args
                    .get(index)
                    .with_context(|| format!("Missing value for {key}"))?;
                index += 1;
                if key == "--root" {
                    request.root = PathBuf::from(value);
                    continue;
                }
                let value = string(value)?.to_owned();
                match key {
                    "--component" => request.component = Some(value),
                    "--app" => request.app = Some(value),
                    "--package" => request.package = Some(value),
                    "--expected-manifest" => request.expected_manifest = Some(value),
                    "--headless" => request.headless = boolean(&value, "headless")?,
                    "--follow" => request.follow = boolean(&value, "follow")?,
                    "--tail" => request.tail = value.parse().context("Invalid logs tail")?,
                    "--network" if command == "exec" => network = value,
                    _ => bail!("Option {key} is not valid here"),
                }
            }
            _ => bail!("Unknown option: {key}"),
        }
    }
    if command == "exec" {
        if argv.is_empty()
            && let Some(value) = env_string("DEVKIT_EXEC_COMMAND")
        {
            argv = shell_words::split(&value)
                .context("Invalid command= quoting; use native exec -- ARGV for exact arguments")?
                .into_iter()
                .map(Into::into)
                .collect();
        }
        return runtime::exec(
            ExecRequest {
                runtime: request,
                argv,
                write,
                network,
                prepare,
            },
            runner,
        );
    }
    ensure!(argv.is_empty(), "Unexpected trailing arguments");
    if command == "describe" {
        let lock = crate::state::CheckoutLock::acquire(&request.root)?;
        let tool = runtime::ToolAdmission::capture(&request)?;
        let full =
            crate::state::admit_runtime_snapshot(&tool.root, tool.state, tool.manifest_identity)?;
        println!(
            "{}",
            serde_json::to_string(
                &serde_json::json!({"version":full.state.version,"generation":full.state.generation,"components":full.state.components,"inputs":full.native.inputs})
            )?
        );
        drop(lock);
        return Ok(());
    }
    if command == "run" {
        ensure!(
            [
                "help", "doctor", "setup", "check", "fix", "test", "build", "package", "dev", "up",
                "down", "logs", "status"
            ]
            .contains(&request.command.as_str()),
            "Unknown Make task: {}",
            request.command
        );
        let mut make = Command::new("make");
        make.arg("--no-print-directory")
            .arg("-C")
            .arg(&request.root)
            .arg(&request.command);
        if let Some(app) = request.app {
            crate::files::slug(&app)?;
            make.arg(format!("app={app}"));
        }
        if let Some(package) = request.package {
            crate::files::slug(&package)?;
            make.arg(format!("package={package}"));
        }
        make.arg(format!("headless={}", u8::from(request.headless)))
            .arg(format!("follow={}", u8::from(request.follow)))
            .arg(format!("tail={}", request.tail));
        make.env(
            "DEVKIT_ENGINE_PATH",
            fs::canonicalize(std::env::current_exe()?)?,
        );
        runner.check_cancelled()?;
        // Make owns its signal/exit behavior and the actual DAG. Native leaves
        // supervise their own children; no second controller kills their cleanup.
        use std::os::unix::process::CommandExt;
        return Err(make.exec()).context("Cannot execute root Makefile");
    }
    runtime::run(request, runner)
}
fn internal(args: &[OsString], runner: &ProcessRunner) -> Result<()> {
    let command = args
        .first()
        .map(|a| string(a))
        .transpose()?
        .context("Expected internal command")?;
    match command {
        "rust" => {
            ensure!(args.len() == 2, "internal rust expects a task command");
            crate::rust_gate::run(string(&args[1])?, runner)
        }
        "desktop" => {
            ensure!(args.len() == 1, "internal desktop takes no arguments");
            crate::desktop::run(runner)
        }
        "node" => {
            ensure!(
                args.len() >= 4 && args[2] == "--",
                "internal node expects VERSION -- ARGV"
            );
            let output = runner.capture(Command::new("node").arg("--version"))?;
            ensure!(
                output.text()?.trim() == format!("v{}", string(&args[1])?),
                "Unexpected Node version: {}",
                output.text()?.trim()
            );
            fs::create_dir_all(std::env::var_os("HOME").context("Missing Node HOME")?)?;
            let mut command = Command::new(&args[3]);
            command.args(&args[4..]);
            runner.checked(&mut command, OutputMode::Inherit)?;
            Ok(())
        }
        "guard" => {
            ensure!(
                args.len() >= 5 && args[3] == "--",
                "internal guard expects NAME TOKEN -- ARGV"
            );
            let owned = processes::OwnedContainer::new(
                string(&args[1])?.to_owned(),
                string(&args[2])?.to_owned(),
            );
            let mut command = Command::new(&args[4]);
            command.args(&args[5..]);
            runner.run_owned(&mut command, OutputMode::Inherit, &owned)?;
            Ok(())
        }
        _ => bail!("Unknown internal command: {command}"),
    }
}
