fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    #[cfg(target_os = "macos")]
    let handoff = {
        let value = std::env::var_os(devkit::processes::PARENT_HANDOFF);
        // SAFETY: process entry, before signals, threads or native libraries are initialized.
        unsafe { std::env::remove_var(devkit::processes::PARENT_HANDOFF) };
        if args.first().is_some_and(|v| v == "internal")
            && args.get(1).is_some_and(|v| v == "watch-group")
        {
            return if devkit::processes::run_watcher(&args[2..]).is_ok() {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::FAILURE
            };
        }
        value
    };
    let json = devkit::cli::wants_json(&args);
    let command = devkit::cli::command_label(&args);
    let result = (|| -> devkit::Result<()> {
        let runner = devkit::processes::ProcessRunner::new()?;
        #[cfg(target_os = "macos")]
        {
            runner.enable_group_watcher(std::env::current_exe()?)?;
            devkit::processes::watch_cli_parent(&runner, handoff.as_deref())?;
        }
        #[cfg(target_os = "linux")]
        devkit::processes::watch_parent(&runner)?;
        let result = devkit::cli::main(args, &runner);
        if result.as_ref().err().is_some_and(|e| {
            e.chain().any(|e| {
                e.downcast_ref::<devkit::processes::CommandFailure>()
                    .is_some()
                    || e.downcast_ref::<devkit::cli::SilentExit>().is_some()
            })
        }) {
            return result;
        }
        if let Err(cancelled) = runner.check_cancelled() {
            return Err(if let Err(error) = result {
                cancelled.context(format!("{error:#}"))
            } else {
                cancelled
            });
        }
        result
    })();
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            if let Some(silent) = error
                .chain()
                .find_map(|e| e.downcast_ref::<devkit::cli::SilentExit>())
            {
                return std::process::ExitCode::from(silent.0);
            }
            let code = devkit::processes::exit_code(&error);
            if json {
                println!(
                    "{}",
                    serde_json::json!({"schemaVersion":1,"ok":false,"command":command,
                    "error":{"code":devkit::cli::error_code(&error),"message":format!("{error:#}")},"exitCode":code})
                );
            } else {
                eprintln!("devkit: {error:#}");
            }
            std::process::ExitCode::from(code)
        }
    }
}
