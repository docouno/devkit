//! Host-local stand state. Checkout locks precede the registry lock; Docker is
//! never called while the registry is locked.
pub mod epoch;
pub mod ledger;

use crate::{Result, files::*};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Command,
};

pub fn branch_name(root: &Path, runner: &crate::processes::ProcessRunner) -> Result<String> {
    let fallback = || {
        root.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    if !exists(&root.join(".git"))? {
        return Ok(fallback());
    }
    let query = |args: &[&str]| -> Result<String> {
        let mut command = Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        command
            .current_dir(root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(["-c", "core.fsmonitor=false"])
            .args(args);
        Ok(runner
            .checked_with_timeout(
                &mut command,
                crate::processes::OutputMode::Capture(65536),
                std::time::Duration::from_secs(3),
            )?
            .text()?
            .trim()
            .into())
    };
    let branch = query(&["branch", "--show-current"])?;
    if !branch.is_empty() {
        return Ok(branch);
    }
    Ok(format!(
        "detached-{}",
        query(&["rev-parse", "--short", "HEAD"])?
    ))
}

/// Compose's unique-resource merge would retain the ephemeral binding. Replace
/// the whole ports sequence, preserving every normalized declaration instead.
pub fn port_overlay(ports: &ports::Ports, active: &BTreeSet<String>) -> Result<Vec<u8>> {
    let mut services: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for group in &ports.groups {
        let b = &group.binding;
        if !active.contains(&b.service) {
            continue;
        }
        let mut value = json!({"target":b.target,"published":if group.first == group.last {group.first.to_string()} else {format!("{}-{}",group.first,group.last)},"protocol":b.protocol,"mode":b.mode});
        if !b.host_ip.is_empty() {
            value["host_ip"] = json!(b.host_ip);
        }
        if let Some(name) = &b.name {
            value["name"] = json!(name);
        }
        if let Some(protocol) = &b.app_protocol {
            value["app_protocol"] = json!(protocol);
        }
        services.entry(&b.service).or_default().push(value);
    }
    let mut yaml = String::from("services:\n");
    for service in active {
        yaml += &format!(
            "  {}:\n    labels: {}\n",
            serde_json::to_string(service)?,
            serde_json::to_string(&json!({ledger::OWNER_LABEL: ports.owner.key}))?
        );
        if let Some(bindings) = services.get(service.as_str()) {
            yaml += &format!(
                "    ports: !override {}\n",
                serde_json::to_string(bindings)?
            );
        }
    }
    Ok(yaml.into_bytes())
}

pub fn save_overlay(root: &Path, bytes: &[u8]) -> Result<std::path::PathBuf> {
    let path = safe_path(root, ".devkit/state/compose.stand.yaml")?;
    write_atomic(&path, bytes, 0o644)?;
    Ok(path)
}
pub fn clear_overlay(root: &Path) -> Result<()> {
    let path = safe_path(root, ".devkit/state/compose.stand.yaml")?;
    if exists(&path)? {
        anyhow::ensure!(
            std::fs::symlink_metadata(&path)?.is_file(),
            "Unsafe stand overlay during cleanup"
        );
        remove_file(&path)?;
    }
    Ok(())
}
pub mod ports;
