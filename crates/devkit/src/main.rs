fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let json = devkit::cli::wants_json(&args);
    let command = devkit::cli::command_label(&args);
    let result = (|| -> devkit::Result<()> {
        let runner = devkit::processes::ProcessRunner::new()?;
        devkit::processes::watch_parent()?;
        let result = devkit::cli::main(args, &runner);
        if result.as_ref().err().is_some_and(|e| {
            e.chain().any(|e| {
                e.downcast_ref::<devkit::processes::CommandFailure>()
                    .is_some()
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
