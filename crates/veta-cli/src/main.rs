//! The `veta` binary (contract §13).

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match veta_cli::run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("veta: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}
