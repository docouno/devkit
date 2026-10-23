fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--version"] || args == ["version"] {
        println!("devkit {}", env!("CARGO_PKG_VERSION"));
        return std::process::ExitCode::SUCCESS;
    }
    eprintln!("devkit: the native command facade has not been integrated");
    std::process::ExitCode::FAILURE
}
