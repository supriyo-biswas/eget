use std::process::ExitCode;

fn main() -> ExitCode {
    match eget::cli::run(std::env::args_os().collect()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("eget: {error:#}");
            ExitCode::FAILURE
        }
    }
}
