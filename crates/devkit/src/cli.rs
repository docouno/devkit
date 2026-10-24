//! Public CLI policy and output. Runtime streams keep their original Make/argv
//! process boundary; JSON is a bounded lifecycle/metadata protocol.
use crate::{
    Result,
    cache::{self, ReleaseCache},
    delivery, distribution,
    files::*,
    processes::ProcessRunner,
    state,
};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fmt, fs,
    path::PathBuf,
};

const HELP: &str = "Devkit — pinned development tools for Linux x86_64 and Docker Compose\n\nUsage:\n  devkit init TARGET --app NAME=TEMPLATE [--app NAME=TEMPLATE ...]\n  devkit init TARGET --preset fullstack-desktop\n  devkit add-app [TARGET] --name NAME --template TEMPLATE [--use CLIENT]\n  devkit add-package [TARGET] --name NAME --template react-client\n  devkit register [TARGET] (--app NAME | --package NAME) --profile PROFILE\n  devkit update [TARGET] --to VERSION [--preview]\n  devkit recover [TARGET]\n  devkit run COMMAND [--root PATH] [--app NAME | --package NAME]\n  devkit exec --component apps/NAME [--root PATH] [--write] [--network none|bridge] [--prepare] -- ARGV\n  devkit describe [--root PATH]\n  devkit catalog\n  devkit --version\n\nTemplates: node-api, rust-api, react-web, electron-react; package: react-client.\nProfiles: node, web, electron, react-library, rust.\nCommands: setup, doctor, check, fix, test, build, dev, up, down, logs, status, package.\n\nInit pins the selected release. Add/register use the accepted project release;\nupdate requires --to. Recover uses the local journal without a release cache.\n\nOptions:\n  --to VERSION                 Exact release for init/update\n  --package-source registry|local\n  --name NAME                  Explicit project name for init\n  --release-root PATH          Verified local release (author/preview use)\n  --cache-dir PATH             Absolute cache directory; also DEVKIT_CACHE_DIR\n  --offline                    Refuse network acquisition\n  --interactive                Permit terminal prompts for missing input\n  --non-interactive            Require complete arguments\n  --json                       One JSON result for lifecycle/metadata commands\n\nComplete arguments never prompt. Without arguments a terminal opens the create\nwizard. CI and non-terminal input require complete arguments. JSON implies\nnon-interactive; streaming run/exec/task/internal reject --json before execution.\nRun preserves the real root Make DAG. Exec ARGV after -- is passed unchanged.\nMake exec command= uses shellwords quoting without variable/command expansion.\n";

#[derive(Debug)]
pub struct CliError {
    pub code: &'static str,
    pub message: String,
}
impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for CliError {}
pub fn error(code: &'static str, message: impl Into<String>) -> anyhow::Error {
    CliError {
        code,
        message: message.into(),
    }
    .into()
}
pub fn error_code(error: &anyhow::Error) -> &'static str {
    error
        .chain()
        .find_map(|e| e.downcast_ref::<CliError>().map(|e| e.code))
        .unwrap_or("OPERATION_FAILED")
}
pub fn wants_json(args: &[OsString]) -> bool {
    args.iter()
        .take_while(|a| *a != "--")
        .any(|a| a == "--json")
}
pub fn command_label(args: &[OsString]) -> String {
    Globals::parse(args.to_vec())
        .ok()
        .map(|globals| {
            match globals
                .args
                .first()
                .and_then(|value| value.to_str())
                .unwrap_or("init")
            {
                "--version" => "version".to_owned(),
                "--help" | "-h" => "help".to_owned(),
                value => value.to_owned(),
            }
        })
        .unwrap_or_else(|| "devkit".to_owned())
}
pub(crate) fn string(value: &OsStr) -> Result<&str> {
    value.to_str().context("Expected a UTF-8 option value")
}

pub(crate) struct Globals {
    pub json: bool,
    pub interactive: bool,
    pub non_interactive: bool,
    pub offline: bool,
    pub cache_dir: Option<PathBuf>,
    pub args: Vec<OsString>,
}
impl Globals {
    fn parse(args: Vec<OsString>) -> Result<Self> {
        let mut result = Self {
            json: false,
            interactive: false,
            non_interactive: false,
            offline: false,
            cache_dir: None,
            args: Vec::new(),
        };
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if arg == "--" {
                result.args.push(arg);
                result.args.extend(args);
                break;
            }
            let flag = match arg.to_str() {
                Some("--json") => Some(&mut result.json),
                Some("--interactive") => Some(&mut result.interactive),
                Some("--non-interactive") => Some(&mut result.non_interactive),
                Some("--offline") => Some(&mut result.offline),
                _ => None,
            };
            if let Some(value) = flag {
                ensure!(!*value, "Repeated option: {}", arg.to_string_lossy());
                *value = true;
            } else if arg == "--cache-dir" {
                ensure!(result.cache_dir.is_none(), "Repeated option: --cache-dir");
                result.cache_dir = Some(PathBuf::from(
                    args.next().context("Missing value for --cache-dir")?,
                ));
            } else {
                result.args.push(arg);
            }
        }
        ensure!(
            !(result.interactive && (result.non_interactive || result.json)),
            "--interactive cannot be combined with --json or --non-interactive"
        );
        result.non_interactive |= result.json;
        Ok(result)
    }
    pub fn emit(&self, command: &str, result: Value, human: &str) {
        if self.json {
            println!(
                "{}",
                json!({"schemaVersion":1,"ok":true,"command":command,"result":result})
            );
        } else {
            println!("{human}");
        }
    }
}

pub(crate) struct Options {
    pub target: Option<PathBuf>,
    values: BTreeMap<String, Vec<OsString>>,
}
impl Options {
    pub fn parse(
        args: &[OsString],
        allowed: &[&str],
        repeat: &[&str],
        switches: &[&str],
    ) -> Result<Self> {
        let mut result = Self {
            target: None,
            values: BTreeMap::new(),
        };
        let mut index = 0;
        let mut positional = false;
        while index < args.len() {
            let arg = &args[index];
            index += 1;
            if !positional && arg == "--" {
                positional = true;
                continue;
            }
            if !positional && arg.to_str().is_some_and(|s| s.starts_with('-')) {
                let key = string(arg)?;
                ensure!(allowed.contains(&key), "Unknown option: {key}");
                ensure!(
                    !result.values.contains_key(key) || repeat.contains(&key),
                    "Repeated option: {key}"
                );
                let value = if switches.contains(&key) {
                    OsString::from("1")
                } else {
                    let value = args
                        .get(index)
                        .with_context(|| format!("Missing value for {key}"))?;
                    ensure!(
                        !value.to_str().is_some_and(|v| v.starts_with("--")),
                        "Missing value for {key}"
                    );
                    index += 1;
                    value.clone()
                };
                result.values.entry(key.to_owned()).or_default().push(value);
            } else {
                ensure!(
                    result.target.is_none(),
                    "Expected only one target project path"
                );
                result.target = Some(PathBuf::from(arg));
            }
        }
        Ok(result)
    }
    pub fn value(&self, key: &str) -> Result<Option<&str>> {
        self.values.get(key).map(|v| string(&v[0])).transpose()
    }
    pub fn path(&self, key: &str) -> Option<PathBuf> {
        self.values.get(key).map(|v| PathBuf::from(&v[0]))
    }
    pub fn all(&self, key: &str) -> Result<Vec<String>> {
        self.values
            .get(key)
            .map(|v| v.iter().map(|v| string(v).map(str::to_owned)).collect())
            .unwrap_or_else(|| Ok(Vec::new()))
    }
    pub fn set(&mut self, key: &str, value: impl Into<OsString>) {
        self.values.insert(key.to_owned(), vec![value.into()]);
    }
    pub fn add(&mut self, key: &str, value: impl Into<OsString>) {
        self.values
            .entry(key.to_owned())
            .or_default()
            .push(value.into());
    }
    pub fn flag(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }
    pub fn required(&self, key: &str) -> Result<&str> {
        self.value(key)?
            .ok_or_else(|| error("MISSING_ARGUMENT", format!("Missing required option {key}")))
    }
    pub fn required_path(&self, key: &str) -> Result<PathBuf> {
        self.path(key)
            .ok_or_else(|| error("MISSING_ARGUMENT", format!("Missing required option {key}")))
    }
}

pub fn main(args: Vec<OsString>, runner: &ProcessRunner) -> Result<()> {
    let globals = Globals::parse(args)?;
    if globals
        .args
        .iter()
        .take_while(|a| *a != "--")
        .any(|a| a == "--help" || a == "-h")
        || globals.args.first().is_some_and(|a| a == "help")
    {
        if globals.json {
            return Err(error(
                "UNSUPPORTED_OUTPUT_MODE",
                "Help is text; use catalog --json for machine-readable templates",
            ));
        }
        print!("{HELP}");
        return Ok(());
    }
    let command = globals
        .args
        .first()
        .map(|v| string(v))
        .transpose()?
        .unwrap_or("init");
    if ["--version", "version"].contains(&command) {
        ensure!(globals.args.len() == 1, "Version takes no arguments");
        globals.emit(
            "version",
            json!({"version":env!("CARGO_PKG_VERSION"),"target":distribution::build_info().target}),
            &format!("devkit {}", env!("CARGO_PKG_VERSION")),
        );
        return Ok(());
    }
    if command == "catalog" {
        ensure!(globals.args.len() == 1, "Catalog takes no arguments");
        let mut entries = Vec::new();
        for name in [
            "node-api",
            "rust-api",
            "react-web",
            "electron-react",
            "react-client",
        ] {
            let template = crate::catalog::template(name)?;
            entries.push(json!({"id":name,"role":template.role,"toolchain":template.toolchain,"profile":template.profile}));
        }
        globals.emit("catalog", json!({"templates":entries,"presets":["fullstack-desktop"]}), "Applications: node-api, rust-api, react-web, electron-react\nShared packages: react-client\nPreset: fullstack-desktop (Rust API, shared React web and Electron)");
        return Ok(());
    }
    if ["run", "exec", "task", "internal"].contains(&command) {
        if globals.json {
            return Err(error(
                "UNSUPPORTED_OUTPUT_MODE",
                format!("{command} streams native output and does not support --json"),
            ));
        }
        ensure!(
            !globals.interactive,
            "Streaming commands do not accept --interactive"
        );
        ensure!(
            !globals.offline,
            "--offline applies to lifecycle release acquisition; daily tasks already use the accepted local engine"
        );
        if command == "internal" && internal(&globals.args[1..], runner)? {
            return Ok(());
        }
        if ["exec", "task"].contains(&command) {
            dispatch_accepted(&globals, runner)?;
        }
        return crate::runtime_cli::main(
            globals.args.clone(),
            runner,
            globals.cache_dir.as_deref(),
        );
    }
    if command == "describe" {
        let options = Options::parse(&globals.args[1..], &["--root"], &[], &[])?;
        ensure!(options.target.is_none(), "Describe uses --root PATH");
        let root = project_root(&options.path("--root").unwrap_or(std::env::current_dir()?))?;
        let _lock = state::CheckoutLock::acquire(&root)?;
        let manifest = identity(&safe_path(&root, state::MANIFEST_PATH)?)?
            .context("Missing project manifest")?;
        let state = state::read_project_state(&root)?;
        let full = state::admit_runtime_snapshot(&root, state, manifest)?;
        let result = json!({"root":full.root,"version":full.state.version,"releaseIdentity":full.state.release_identity,"generation":full.state.generation,"components":full.state.components,"inputs":full.native.inputs});
        full.recheck()?;
        runner.check_cancelled()?;
        if globals.json {
            globals.emit("describe", result, "");
        } else {
            println!("{result}");
        }
        return Ok(());
    }
    ensure!(
        [
            "init",
            "add-app",
            "add-package",
            "register",
            "update",
            "recover"
        ]
        .contains(&command),
        "Unknown command: {command}"
    );
    crate::lifecycle_cli::run(command, &globals, runner)
}

fn dispatch_accepted(globals: &Globals, runner: &ProcessRunner) -> Result<()> {
    let before_separator = globals
        .args
        .iter()
        .position(|a| a == "--")
        .unwrap_or(globals.args.len());
    let mut root = std::env::current_dir()?;
    let mut seen = false;
    for index in 1..before_separator {
        if globals.args[index] == "--root" {
            ensure!(!seen, "Repeated option: --root");
            seen = true;
            root = PathBuf::from(
                globals
                    .args
                    .get(index + 1)
                    .filter(|_| index + 1 < before_separator)
                    .context("Missing value for --root")?,
            );
        }
    }
    let root = project_root(&root)?;
    let state = state::read_project_state(&root)?;
    ensure!(
        !cache::legacy(&state.version),
        "Native exec/task requires Devkit 0.3 or newer; use run for legacy Make tasks or update --to VERSION"
    );
    let name = ".devkit/runtime/toolchains.env";
    let path = safe_path(&root, name)?;
    ensure!(
        state.managed.get(name) == identity(&path)?.as_ref(),
        "Managed toolchain pin conflict"
    );
    let pins = crate::runtime::ToolPins::parse(&fs::read(path)?)?;
    ensure!(
        pins.version == state.version,
        "Accepted version differs from toolchain pins"
    );
    let pin = pins.engine.context("Missing accepted native engine pin")?;
    let current = fs::canonicalize(std::env::current_exe()?)?;
    if delivery::file_digest(&current)? == pin.sha256 {
        ensure!(
            pin.version == env!("CARGO_PKG_VERSION"),
            "Accepted engine version differs from its build"
        );
        return Ok(());
    }
    let cache = ReleaseCache::new(globals.cache_dir.as_deref(), runner)?;
    let engine = cache.accepted_engine(&pin)?;
    ensure!(
        engine != current && std::env::var_os("DEVKIT_DISPATCHED_ENGINE").is_none(),
        "Accepted engine dispatch did not converge"
    );
    runner.check_cancelled()?;
    let mut command = std::process::Command::new(&engine);
    if let Some(path) = &globals.cache_dir {
        command.arg("--cache-dir").arg(path);
    }
    command
        .args(&globals.args)
        .env("DEVKIT_DISPATCHED_ENGINE", &pin.sha256);
    use std::os::unix::process::CommandExt;
    Err(command.exec()).context("Cannot execute the accepted native engine")
}

fn internal(args: &[OsString], runner: &ProcessRunner) -> Result<bool> {
    let Some(command) = args.first().map(|v| string(v)).transpose()? else {
        return Ok(false);
    };
    match command {
        "build-info" => {
            ensure!(args.len() == 1, "Build-info takes no arguments");
            println!("{}", serde_json::to_string(&distribution::build_info())?);
        }
        "fetch" => {
            let options =
                Options::parse(&args[1..], &["--url", "--output", "--max-bytes"], &[], &[])?;
            ensure!(options.target.is_none(), "Fetch takes named options only");
            crate::download::fetch(
                options.required("--url")?,
                &options.required_path("--output")?,
                options.required("--max-bytes")?.parse()?,
                runner,
            )?;
        }
        "assemble" => {
            let options = Options::parse(
                &args[1..],
                &[
                    "--root",
                    "--engine",
                    "--npm-inputs",
                    "--output",
                    "--source-commit",
                    "--source-tree",
                    "--build-run",
                    "--notices",
                    "--local",
                ],
                &[],
                &["--local"],
            )?;
            ensure!(
                options.target.is_none(),
                "Assemble takes named options only"
            );
            let index = crate::assembler::assemble(
                &crate::assembler::AssemblyOptions {
                    root: options.required_path("--root")?,
                    engine: options.required_path("--engine")?,
                    npm_inputs: options.required_path("--npm-inputs")?,
                    output: options.required_path("--output")?,
                    notices: options.required_path("--notices")?,
                    source_commit: options.required("--source-commit")?.to_owned(),
                    source_tree: options.required("--source-tree")?.to_owned(),
                    build_run: options.required("--build-run")?.to_owned(),
                    local: options.flag("--local"),
                },
                runner,
            )?;
            println!("{}", serde_json::to_string(&index)?);
        }
        "collect-notices" => {
            let options = Options::parse(
                &args[1..],
                &[
                    "--root",
                    "--cargo-home",
                    "--rust-docs",
                    "--output",
                    "--target",
                    "--rust-version",
                ],
                &[],
                &[],
            )?;
            ensure!(
                options.target.is_none(),
                "Collect-notices takes named options only"
            );
            let result = crate::notices::collect_notices(
                &crate::notices::NoticeOptions {
                    root: options.required_path("--root")?,
                    cargo_home: options.required_path("--cargo-home")?,
                    rust_docs: options.required_path("--rust-docs")?,
                    output: options.required_path("--output")?,
                    target: options
                        .value("--target")?
                        .unwrap_or(distribution::TARGET)
                        .to_owned(),
                    rust_version: options.required("--rust-version")?.to_owned(),
                },
                runner,
            )?;
            println!("{result}");
        }
        _ => return Ok(false),
    }
    Ok(true)
}
