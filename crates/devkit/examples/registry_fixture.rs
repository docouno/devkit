//! Author-only native lifecycle entry with a local npm registry fixture.
//! The shipped CLI does not expose this test registry override.
use devkit::{
    Result,
    model::PackageMode,
    project::{DockerNpmResolver, InitOptions, UpdateOptions, init_project, update_project},
};
use std::path::PathBuf;
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        args.len() == 4,
        "Expected init|update RELEASE TARGET REGISTRY"
    );
    let release = PathBuf::from(&args[1]);
    let target = PathBuf::from(&args[2]);
    match args[0].as_str() {
        "init" => {
            init_project(
                &release,
                &target,
                &InitOptions {
                    template: Some("node-api".into()),
                    package_mode: Some(PackageMode::Registry),
                    registry: Some(&args[3]),
                    ..Default::default()
                },
                &DockerNpmResolver,
            )?;
        }
        "update" => {
            update_project(
                &release,
                &target,
                &UpdateOptions {
                    registry: Some(&args[3]),
                    ..Default::default()
                },
                &DockerNpmResolver,
            )?;
        }
        _ => anyhow::bail!("Expected init or update"),
    }
    Ok(())
}
