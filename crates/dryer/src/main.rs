use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = dryer_core::cli::run(&args);
    ExitCode::from((code & 0xff) as u8)
}
