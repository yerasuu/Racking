use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: racking-server --config PATH\n       racking-server --help-api   (print the API guide)\n";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config_path = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["--config", path] => PathBuf::from(path),
        ["--help-api"] => {
            print!("{}", racking_server::guide());
            return ExitCode::SUCCESS;
        }
        ["--version"] => {
            println!("racking-server {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        ["--help" | "-h"] => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let config = match racking_server::Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(target: "racking::server", error = %e, "bad config");
            return ExitCode::FAILURE;
        }
    };
    match racking_server::start(&config) {
        Ok(running) => {
            tracing::info!(
                target: "racking::server",
                addr = %running.addr(),
                data_dir = %config.data_dir.display(),
                agents = config.agents.len(),
                "listening"
            );
            running.join();
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(target: "racking::server", error = %e, "cannot start");
            ExitCode::FAILURE
        }
    }
}
