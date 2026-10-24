use crate::{
    Result,
    processes::{OutputMode, ProcessRunner},
};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

pub fn display_overlay(service: &str, headless: bool) -> Result<Value> {
    let mut spec = json!({"environment":{"DEVKIT_HEADLESS":if headless {"1"} else {"0"}}});
    if !headless {
        let display = std::env::var("DISPLAY").unwrap_or_default();
        let valid = regex::Regex::new(r"^:[0-9]+(?:\.[0-9]+)?$")?;
        let authority = std::env::var_os("XAUTHORITY")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(&std::env::var_os("HOME").unwrap_or_default()).join(".Xauthority")
            });
        ensure!(
            valid.is_match(&display) && Path::new("/tmp/.X11-unix").is_dir() && authority.is_file(),
            "desktop requires local DISPLAY and readable XAUTHORITY; use headless=1 for a virtual display"
        );
        fs::File::open(&authority).context("Cannot read XAUTHORITY")?;
        let authority = fs::canonicalize(authority)?;
        spec["environment"]["DISPLAY"] = json!(display);
        spec["environment"]["XAUTHORITY"] = json!("/run/devkit-xauthority");
        spec["volumes"] = json!([{"type":"bind","source":"/tmp/.X11-unix","target":"/tmp/.X11-unix","read_only":true,"bind":{"create_host_path":false}},{"type":"bind","source":authority,"target":"/run/devkit-xauthority","read_only":true,"bind":{"create_host_path":false}}]);
    }
    Ok(json!({"services":{service:spec}}))
}
pub fn run(runner: &ProcessRunner) -> Result<()> {
    let home = std::env::var_os("HOME").context("Missing desktop HOME")?;
    fs::create_dir_all(home)?;
    let mut command = if std::env::var("DEVKIT_HEADLESS").as_deref() == Ok("1") {
        let mut c = Command::new("xvfb-run");
        c.args(["-a", "-s", "-screen 0 1280x800x24", "npm", "run", "dev"]);
        c
    } else {
        ensure!(
            !std::env::var("DISPLAY").unwrap_or_default().is_empty(),
            "Desktop development needs DISPLAY; use headless=1 for a virtual display"
        );
        let mut c = Command::new("npm");
        c.args(["run", "dev"]);
        c
    };
    runner.checked(&mut command, OutputMode::Inherit)?;
    Ok(())
}
