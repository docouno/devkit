//! Author-only GitHub/npm release controller. This binary is never assembled
//! into the consumer payload; `devkit` remains the sole product executable.
mod release_ops;

fn main() -> std::process::ExitCode {
    match release_ops::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            std::process::ExitCode::from(devkit::processes::exit_code(&error))
        }
    }
}
