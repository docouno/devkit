fn main() -> std::process::ExitCode {
    let result = (|| -> devkit::Result<()> {
        let runner = devkit::processes::ProcessRunner::new()?;
        devkit::processes::watch_parent()?;
        devkit::cli::main(std::env::args_os().skip(1).collect(), &runner)?;
        runner.check_cancelled()
    })();
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("devkit: {error:#}");
            std::process::ExitCode::from(devkit::processes::exit_code(&error))
        }
    }
}
