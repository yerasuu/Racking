use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use racking_server::{Bootstrap, Redeemed, Shutdown, Waited};

const USAGE: &str = "usage: racking-server --config PATH
       racking-server invite --config PATH [--role worker|main] [--name NAME] [--no-wait]
       racking-server invite --config PATH --resume ID
       racking-server --help-api   (print the API guide)

`invite` makes a pairing code for the first agent on the server machine itself,
with no admin token: it opens the database in the config's data_dir, which only
the owner can. It waits for `rack join CODE` on the other machine, shows the
fingerprint and asks on this terminal whether both screens show the same words.
--no-wait prints the code and stops; --resume ID waits again for that invite.
--yes-if-fingerprint WORDS answers for you: for scripted tests only.
";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("invite") {
        return invite(&args[1..]);
    }
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
            if let Err(e) = on_signal(running.shutdown_handle()) {
                tracing::error!(target: "racking::server", error = %e, "cannot handle SIGTERM and SIGINT");
                return ExitCode::FAILURE;
            }
            let stopped = running.shutdown();
            tracing::info!(
                target: "racking::server",
                abandoned = stopped.abandoned,
                checkpointed = stopped.checkpointed,
                "stopped"
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(target: "racking::server", error = %e, "cannot start");
            ExitCode::FAILURE
        }
    }
}

/// The first SIGTERM or SIGINT starts a graceful shutdown; a second one exits at
/// once with 128 + the signal, as a shell reports a process killed by it.
#[cfg(unix)]
fn on_signal(shutdown: Shutdown) -> std::io::Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::flag;
    use signal_hook::iterator::Signals;

    let armed = Arc::new(AtomicBool::new(false));
    for signal in [SIGTERM, SIGINT] {
        // The exit must be registered before the flag that arms it, or the first signal would already find it
        // armed ([signal-hook: register_conditional_shutdown](https://docs.rs/signal-hook/0.4.5/signal_hook/flag/fn.register_conditional_shutdown.html)).
        flag::register_conditional_shutdown(signal, 128 + signal, Arc::clone(&armed))?;
        flag::register(signal, Arc::clone(&armed))?;
    }
    let mut signals = Signals::new([SIGTERM, SIGINT])?;
    std::thread::Builder::new().name("signals".to_owned()).spawn(move || {
        if let Some(signal) = signals.forever().next() {
            tracing::info!(target: "racking::server", signal, "signal received; a second one exits at once");
            shutdown.begin();
        }
    })?;
    Ok(())
}

/// The server runs on Unix only; elsewhere it serves until killed.
#[cfg(not(unix))]
fn on_signal(_shutdown: Shutdown) -> std::io::Result<()> {
    Ok(())
}

/// The options of `invite`, read once.
struct InviteArgs {
    config: PathBuf,
    role: String,
    name: Option<String>,
    resume: Option<i64>,
    no_wait: bool,
    yes_if: Option<String>,
}

fn invite_args(args: &[String]) -> Result<InviteArgs, String> {
    let mut parsed = InviteArgs {
        config: PathBuf::new(),
        role: "worker".to_owned(),
        name: None,
        resume: None,
        no_wait: false,
        yes_if: None,
    };
    let mut config = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--config" => config = Some(PathBuf::from(value()?)),
            "--role" => parsed.role = value()?,
            "--name" => parsed.name = Some(value()?),
            "--resume" => {
                parsed.resume = Some(value()?.parse().map_err(|_| "--resume needs an invite id".to_owned())?);
            }
            "--yes-if-fingerprint" => parsed.yes_if = Some(value()?),
            "--no-wait" => parsed.no_wait = true,
            other => return Err(format!("unknown option {other}")),
        }
    }
    parsed.config = config.ok_or("--config PATH is required")?;
    if parsed.resume.is_some() && (parsed.name.is_some() || parsed.no_wait) {
        return Err("--resume takes no --name or --no-wait".to_owned());
    }
    Ok(parsed)
}

fn invite(args: &[String]) -> ExitCode {
    let args = match invite_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprint!("racking-server invite: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let config = match racking_server::Config::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("racking-server invite: {e}");
            return ExitCode::from(2);
        }
    };
    let result = Bootstrap::open(&config).and_then(|owner| {
        let id = match args.resume {
            Some(id) => id,
            None => {
                let (id, code, name, expires) = owner.invite(args.name.as_deref(), &args.role)?;
                println!(
                    "invite {id} for {name} ({}), expires {expires}\ncode: {code}\n\nOn the other machine run:\n  \
                     rack join {code} --server http://{}\n(use this machine's LAN address if the server binds \
                     another)",
                    args.role, config.bind
                );
                if args.no_wait {
                    println!(
                        "then: racking-server invite --config {} --resume {id}",
                        args.config.display()
                    );
                    return Ok(true);
                }
                id
            }
        };
        wait_and_decide(&owner, id, args.yes_if.as_deref())
    });
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("racking-server invite: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Waits for the code to be redeemed, then asks the owner. True once confirmed.
fn wait_and_decide(owner: &Bootstrap, id: i64, yes_if: Option<&str>) -> Result<bool, String> {
    println!("waiting for the code to be redeemed (Ctrl-C stops waiting; resume with --resume {id})...");
    let joined = loop {
        match owner.poll(id)? {
            Waited::Open => std::thread::sleep(Duration::from_secs(1)),
            Waited::Redeemed(r) => break r,
            Waited::Gone(state) => {
                println!("invite {id} is {state}");
                return Ok(state == "confirmed");
            }
        }
    };
    print_joined(&joined);
    let yes = match yes_if {
        Some(words) => same_words(words, &joined.fingerprint),
        None => ask_on_terminal("Does the other screen show the same words? [y/N] ")?,
    };
    owner.decide(id, yes)?;
    if yes {
        println!("linked: {} is confirmed", joined.name);
    } else {
        println!("denied: {} and its token are deleted", joined.name);
    }
    Ok(yes)
}

fn print_joined(r: &Redeemed) {
    println!(
        "redeemed by {} ({}): os {}, rack {}, from {}\nfingerprint: {}\nconfirm before {}",
        r.name, r.role, r.os, r.rack_version, r.peer, r.fingerprint, r.confirm_by
    );
}

/// Words typed with spaces or dashes, in any case, compared with the fingerprint.
fn same_words(typed: &str, fingerprint: &str) -> bool {
    let words = |s: &str| {
        s.split([' ', '-'])
            .filter(|w| !w.is_empty())
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>()
    };
    words(typed) == words(fingerprint)
}

/// The answer comes from the terminal, never from stdin, so a script or an agent
/// piping `y` cannot confirm in the owner's place.
fn ask_on_terminal(question: &str) -> Result<bool, String> {
    let (tty_in, mut tty_out) = open_terminal()
        .map_err(|e| format!("no terminal to ask on ({e}); the joiner is denied. Run this from a terminal"))?;
    tty_out
        .write_all(question.as_bytes())
        .and_then(|()| tty_out.flush())
        .map_err(|e| format!("cannot write to the terminal: {e}"))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(tty_in).read_line(&mut line);
        let _ = tx.send(line);
    });
    // The server denies the joiner when its confirmation time ends, so waiting longer is pointless.
    let answer = rx.recv_timeout(Duration::from_secs(600)).unwrap_or_default();
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

#[cfg(unix)]
fn open_terminal() -> std::io::Result<(std::fs::File, std::fs::File)> {
    let input = std::fs::File::open("/dev/tty")?;
    let output = std::fs::OpenOptions::new().write(true).open("/dev/tty")?;
    Ok((input, output))
}

#[cfg(not(unix))]
fn open_terminal() -> std::io::Result<(std::fs::File, std::fs::File)> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "racking-server runs on Unix only",
    ))
}
