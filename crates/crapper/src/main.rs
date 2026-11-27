use std::process::ExitCode;

fn main() -> ExitCode {
    ub_runtime::init_cli_runtime();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = crapper_lib::cli::run(&args);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
