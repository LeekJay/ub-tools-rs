use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = mutator_lib::crapper_bridge::run_cli(&args);
    ExitCode::from((code & 0xff) as u8)
}
