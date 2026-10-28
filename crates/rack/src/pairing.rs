//! `rack link`, `rack join`, `rack links` and `rack unlink` (#46): the two ends of
//! a pairing. The inviter's `rack link` shows a one-time code and waits; the other
//! machine's `rack join` redeems it and waits too; both screens then show the same
//! four words, and only the human at the inviter's terminal can confirm the link.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::client::{Client, RackError, Result, local};

/// Seconds per long wait on the server; it cuts anything above 55.
const WAIT_SECS: u64 = 25;
/// The server denies a joiner its inviter has not confirmed within 600 s; a little
/// more covers the clocks of two machines.
const CONFIRM_WAIT: Duration = Duration::from_secs(660);

/// How `rack link` ends: confirmed, or why not (exit 1).
fn failed(what: String) -> RackError {
    RackError::Local(what)
}

/// Creates an invite, prints the code and, unless `no_wait`, waits for the joiner.
pub fn invite(
    c: &Client,
    name: Option<&str>,
    role: &str,
    ttl: Option<i64>,
    no_wait: bool,
    yes_if: Option<&str>,
) -> Result<()> {
    let mut body = json!({ "role": role });
    if let Some(n) = name {
        body["name"] = json!(n);
    }
    if let Some(t) = ttl {
        body["ttl_secs"] = json!(t);
    }
    let made: Value = c.post("/admin/invites", Some(&body))?;
    let id = made["id"].as_i64().unwrap_or_default();
    let code = made["code"].as_str().unwrap_or_default();
    println!(
        "invite {id} for {} ({}), expires {}\ncode: {code}\n\nOn the other machine run:\n  rack join {code} --server {}\n\
         Give the code only to that machine's owner, by a channel you trust.",
        made["name"].as_str().unwrap_or("?"),
        made["role"].as_str().unwrap_or("?"),
        made["expires_at"].as_str().unwrap_or("?"),
        c.base()
    );
    if no_wait {
        println!("then: rack link --resume {id}");
        return Ok(());
    }
    wait_and_decide(c, id, yes_if)
}

/// Waits for invite `id` to be redeemed, shows who redeemed it and asks.
pub fn wait_and_decide(c: &Client, id: i64, yes_if: Option<&str>) -> Result<()> {
    let _ = std::io::stdout().flush();
    let mut told = false;
    let invite = loop {
        let invite: Value = c.get(&format!("/admin/invites/{id}?timeout={WAIT_SECS}"))?;
        match invite["state"].as_str().unwrap_or("?") {
            "open" => {
                if !told {
                    eprintln!(
                        "rack: waiting for the code to be redeemed, until {} (Ctrl-C stops waiting; \
                         `rack link --resume {id}` waits again)",
                        invite["expires_at"].as_str().unwrap_or("?")
                    );
                    told = true;
                }
            }
            "redeemed" => break invite,
            "confirmed" => {
                println!("invite {id} is confirmed already");
                return Ok(());
            }
            other => return Err(failed(format!("invite {id} is {other}: no link"))),
        }
    };
    let fingerprint = invite["fingerprint"].as_str().unwrap_or_default();
    println!(
        "redeemed by {} ({}): os {}, rack {}, from {}\nfingerprint: {fingerprint}\nconfirm before {}",
        invite["name"].as_str().unwrap_or("?"),
        invite["role"].as_str().unwrap_or("?"),
        invite["os"].as_str().unwrap_or("?"),
        invite["rack_version"].as_str().unwrap_or("?"),
        invite["peer"].as_str().unwrap_or("?"),
        invite["confirm_by"].as_str().unwrap_or("?"),
    );
    let yes = match yes_if {
        Some(words) => same_words(words, fingerprint),
        None => match ask_on_terminal("Does the other screen show the same words? [y/N] ") {
            Ok(yes) => yes,
            Err(e) => {
                eprintln!("rack: {e}");
                false
            }
        },
    };
    let name = invite["name"].as_str().unwrap_or("?");
    if yes {
        let _: Value = c.post(&format!("/admin/invites/{id}/confirm"), None)?;
        println!("confirmed: linked with {name}");
        Ok(())
    } else {
        let _: Value = c.post(&format!("/admin/invites/{id}/deny"), None)?;
        println!("denied: {name} and its token are deleted");
        Err(failed(format!("invite {id} denied")))
    }
}

/// Words typed with spaces or dashes, in any case, compared with the fingerprint.
pub fn same_words(typed: &str, fingerprint: &str) -> bool {
    let words = |s: &str| {
        s.split([' ', '-'])
            .filter(|w| !w.is_empty())
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>()
    };
    let typed = words(typed);
    !typed.is_empty() && typed == words(fingerprint)
}

/// The answer comes from the terminal, never from stdin, so a script or an agent
/// piping `y` cannot confirm in the owner's place.
fn ask_on_terminal(question: &str) -> Result<bool> {
    let (input, mut output) = open_terminal().map_err(|e| {
        local(
            "no terminal to ask on, so the joiner is denied; run rack link from a terminal",
            e,
        )
    })?;
    output
        .write_all(question.as_bytes())
        .and_then(|()| output.flush())
        .map_err(|e| local("write to the terminal", e))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(input).read_line(&mut line);
        let _ = tx.send(line);
    });
    // No answer before the server's own deadline is a no.
    let answer = rx.recv_timeout(CONFIRM_WAIT).unwrap_or_default();
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

#[cfg(unix)]
fn open_terminal() -> std::io::Result<(File, File)> {
    Ok((
        File::open("/dev/tty")?,
        OpenOptions::new().write(true).open("/dev/tty")?,
    ))
}

#[cfg(windows)]
fn open_terminal() -> std::io::Result<(File, File)> {
    Ok((File::open("CONIN$")?, OpenOptions::new().write(true).open("CONOUT$")?))
}

#[cfg(not(any(unix, windows)))]
fn open_terminal() -> std::io::Result<(File, File)> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no terminal on this platform",
    ))
}

pub fn list_invites(c: &Client) -> Result<()> {
    let all: Value = c.get("/admin/invites")?;
    let invites = all["invites"].as_array().map(Vec::as_slice).unwrap_or_default();
    if invites.is_empty() {
        println!("no pending invites");
    }
    for i in invites {
        let mut line = format!(
            "{} {} {} {}",
            i["id"],
            i["name"].as_str().unwrap_or("?"),
            i["role"].as_str().unwrap_or("?"),
            i["state"].as_str().unwrap_or("?"),
        );
        match i["state"].as_str() {
            Some("redeemed") => line.push_str(&format!(
                ", from {}, confirm before {}: rack link --resume {}",
                i["peer"].as_str().unwrap_or("?"),
                i["confirm_by"].as_str().unwrap_or("?"),
                i["id"]
            )),
            _ => line.push_str(&format!(", expires {}", i["expires_at"].as_str().unwrap_or("?"))),
        }
        println!("{line}");
    }
    Ok(())
}

pub fn cancel(c: &Client, id: i64) -> Result<()> {
    let _: Value = c.delete(&format!("/admin/invites/{id}"))?;
    println!("invite {id} voided: its code no longer works");
    Ok(())
}

pub fn pair(c: &Client, a: &str, b: &str) -> Result<()> {
    let made: Value = c.post("/admin/links", Some(&json!({"a": a, "b": b})))?;
    println!("linked {a} and {b}");
    // A link does not grant sending between workers; the server's hint says which `[permissions]` entry
    // does. A server older than the field sends none, and then there is nothing to add.
    if made["sends"]["a_to_b"] == false || made["sends"]["b_to_a"] == false {
        for line in made["hint"].as_str().unwrap_or_default().lines() {
            eprintln!("note: {line}");
        }
    }
    Ok(())
}

/// `rack links`: one line per agent this one is linked with.
pub fn links(c: &Client) -> Result<()> {
    let mine: Value = c.get("/agent-links")?;
    let list = mine["links"].as_array().map(Vec::as_slice).unwrap_or_default();
    if list.is_empty() {
        println!("not linked with any agent yet");
    }
    for l in list {
        let mut line = format!(
            "{} {}, via {}, {}",
            l["agent"].as_str().unwrap_or("?"),
            l["role"].as_str().unwrap_or("?"),
            l["via"].as_str().unwrap_or("?"),
            l["state"].as_str().unwrap_or("?"),
        );
        if let Some(at) = l["confirmed_at"].as_str() {
            line.push_str(&format!(" {at}"));
        }
        println!("{line}");
    }
    if mine["source"] == "file" {
        println!("(this agent is in the server's config file, so it is linked with every agent)");
    }
    Ok(())
}

pub fn unlink(c: &Client, a: &str, b: &str) -> Result<()> {
    let gone: Value = c.delete(&format!("/admin/links/{a}/{b}"))?;
    println!("unlinked {a} and {b}");
    for name in gone["revoked"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        println!("{name} joined through that link: its token no longer works; its jobs are kept");
    }
    Ok(())
}

/// Where `rack join` writes and how.
pub struct JoinArgs<'a> {
    pub code: &'a str,
    pub server: &'a str,
    pub name: Option<&'a str>,
    pub dir: PathBuf,
    /// The config dir is the default one, so racking-mcp finds `rack.conf` without `RACK_CONFIG`.
    pub default_dir: bool,
    pub force: bool,
}

pub fn join(a: &JoinArgs<'_>) -> Result<()> {
    let base = a.server.trim_end_matches('/');
    for warning in check_server(base)? {
        eprintln!("rack: warning: {warning}");
    }
    let conf = a.dir.join("rack.conf");
    if conf.exists() && !a.force {
        return Err(RackError::Usage(format!(
            "{} exists already; pass --force to replace it (the old one is kept as rack.conf.bak)",
            conf.display()
        )));
    }
    let anonymous = Client::new(base, "")?.identified(env!("CARGO_PKG_VERSION"));
    let mut body = json!({
        "code": a.code,
        "os": std::env::consts::OS,
        "rack_version": env!("CARGO_PKG_VERSION"),
    });
    if let Some(n) = a.name {
        body["name"] = json!(n);
    }
    let joined: Value = anonymous.post("/join", Some(&body))?;
    let token = joined["token"].as_str().unwrap_or_default();
    let backup = write_conf(&conf, base, token, a.force)?;
    println!(
        "joined as {} ({}); credentials saved in {} (mode 600)\nfingerprint: {}\n\
         The other screen must show the same four words. Waiting for {} to confirm, until {}...",
        joined["name"].as_str().unwrap_or("?"),
        joined["role"].as_str().unwrap_or("?"),
        conf.display(),
        joined["fingerprint"].as_str().unwrap_or("?"),
        joined["inviter"].as_str().unwrap_or("the server owner"),
        joined["confirm_by"].as_str().unwrap_or("?"),
    );
    let _ = std::io::stdout().flush();
    let me = Client::new(base, token)?.identified(env!("CARGO_PKG_VERSION"));
    match wait_confirmed(&me) {
        Ok(status) => {
            next_steps(&status, &conf, a.default_dir);
            Ok(())
        }
        Err(e) => {
            // The token is dead or never will work: leave no credentials that look valid.
            if let Err(err) = undo_conf(&conf, backup.as_deref()) {
                eprintln!("rack: {err}");
            }
            Err(e)
        }
    }
}

/// `rack join --resume`: waits again with the credentials `rack join` saved.
pub fn resume_join(c: &Client, conf: Option<&Path>, default_dir: bool) -> Result<()> {
    let status = wait_confirmed(c)?;
    let shown = conf.map_or_else(|| PathBuf::from("rack.conf"), Path::to_path_buf);
    next_steps(&status, &shown, default_dir);
    Ok(())
}

fn wait_confirmed(c: &Client) -> Result<Value> {
    let deadline = Instant::now() + CONFIRM_WAIT;
    loop {
        let status: Value = c.get(&format!("/join/status?timeout={WAIT_SECS}"))?;
        match status["state"].as_str() {
            Some("pending") if Instant::now() < deadline => {}
            Some("confirmed") => {
                println!(
                    "linked with {}",
                    status["inviter"].as_str().unwrap_or("the server owner")
                );
                return Ok(status);
            }
            Some("pending") => return Err(failed("no confirmation in time".to_owned())),
            _ => {
                println!("denied");
                return Err(failed("the link was denied; the token is dead".to_owned()));
            }
        }
    }
}

fn next_steps(status: &Value, conf: &Path, default_dir: bool) {
    let env = if default_dir {
        String::new()
    } else {
        format!("--env RACK_CONFIG={} ", conf.display())
    };
    println!(
        "\nNext steps:\n  1. Register the MCP tools; racking-mcp reads the url and token from rack.conf, so the \
         token never goes on a command line:\n       claude mcp add --scope user {env}--transport stdio racking -- \
         racking-mcp\n  2. Read how to behave: rack guide"
    );
    if status["role"] == "worker" {
        println!(
            "  3. Worker: to let jobs wake a session (rack listen), put \"crossSessionInbound\": \"accept\" only \
             in that session's --settings file, never in the user settings, where it would apply to every \
             session (rack help listen)."
        );
    }
}

/// Writes `url` and `token` to `conf` with mode 600, through a temporary file and
/// a rename so a crash leaves the old file or the new one, never half of one.
/// With `force`, an existing file is first copied to `rack.conf.bak`, whose path is returned.
fn write_conf(conf: &Path, url: &str, token: &str, force: bool) -> Result<Option<PathBuf>> {
    let dir = conf.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|e| local(&format!("create {}", dir.display()), e))?;
    let backup = if conf.exists() && force {
        let bak = conf.with_extension("conf.bak");
        fs::copy(conf, &bak).map_err(|e| local(&format!("back up {}", conf.display()), e))?;
        restrict(&bak)?;
        Some(bak)
    } else {
        None
    };
    let tmp = conf.with_extension(format!("conf.tmp-{}", std::process::id()));
    let mut file = private_file(&tmp)?;
    file.write_all(format!("url = \"{url}\"\ntoken = \"{token}\"\n").as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| local(&format!("write {}", tmp.display()), e))?;
    drop(file);
    fs::rename(&tmp, conf).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        local(&format!("replace {}", conf.display()), e)
    })?;
    Ok(backup)
}

fn undo_conf(conf: &Path, backup: Option<&Path>) -> Result<()> {
    match backup {
        Some(bak) => fs::rename(bak, conf).map_err(|e| local(&format!("restore {}", bak.display()), e)),
        None => fs::remove_file(conf).map_err(|e| local(&format!("remove {}", conf.display()), e)),
    }
}

#[cfg(unix)]
fn private_file(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| local(&format!("create {}", path.display()), e))
}

/// On Windows the file inherits the ACL of the user's profile folder, private to that user.
#[cfg(not(unix))]
fn private_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| local(&format!("create {}", path.display()), e))
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| local(&format!("chmod {}", path.display()), e))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

/// The single check of `--server`: `http://` only, and a host that resolves. A host
/// outside loopback and the private and link-local ranges is allowed with a
/// warning, since Racking has no TLS and is meant for a LAN.
fn check_server(url: &str) -> Result<Vec<String>> {
    let Some(rest) = url.strip_prefix("http://") else {
        return Err(RackError::Usage(format!(
            "--server `{url}` must start with http://, e.g. http://192.168.1.20:7878"
        )));
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.is_empty() || !path.is_empty() {
        return Err(RackError::Usage(format!("--server `{url}`: expected http://HOST:PORT")));
    }
    let with_port = if authority.ends_with(']') || !authority.contains(':') {
        format!("{authority}:80")
    } else {
        authority.to_owned()
    };
    let addrs: Vec<IpAddr> = with_port
        .to_socket_addrs()
        .map_err(|e| RackError::Transport(format!("cannot resolve {authority}: {e}")))?
        .map(|a| a.ip())
        .collect();
    Ok(addrs
        .iter()
        .filter(|ip| !is_local(**ip))
        .map(|ip| {
            format!(
                "{ip} is not a loopback or private LAN address; Racking has no TLS, so the code and the token \
                 cross that network in clear"
            )
        })
        .collect())
}

fn is_local(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_urls() {
        assert!(check_server("http://127.0.0.1:7878").is_ok_and(|w| w.is_empty()));
        assert!(check_server("http://192.168.1.20:7878").is_ok_and(|w| w.is_empty()));
        assert!(check_server("http://[::1]:7878").is_ok_and(|w| w.is_empty()));
        assert!(
            check_server("http://8.8.8.8:7878").is_ok_and(|w| w.len() == 1),
            "public: warned"
        );
        for bad in ["https://192.168.1.20", "192.168.1.20:7878", "http://", "http://h:1/a/b"] {
            assert!(matches!(check_server(bad), Err(RackError::Usage(_))), "{bad}");
        }
    }

    #[test]
    fn words_match_in_any_spelling() {
        assert!(same_words("Able-acid aged  ALSO", "able acid aged also"));
        assert!(!same_words("able acid aged", "able acid aged also"));
        assert!(!same_words("", ""), "no words never confirm");
    }

    #[test]
    fn conf_is_private_atomic_and_backed_up() {
        let dir = std::env::temp_dir().join(format!("rack-pairing-conf-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let conf = dir.join("rack.conf");
        assert_eq!(write_conf(&conf, "http://h:1", "t1", false).expect("write"), None);
        let text = fs::read_to_string(&conf).expect("read");
        assert_eq!(text, "url = \"http://h:1\"\ntoken = \"t1\"\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&conf).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let bak = write_conf(&conf, "http://h:2", "t2", true).expect("replace");
        assert_eq!(
            bak.as_deref().map(fs::read_to_string).and_then(std::result::Result::ok),
            Some(text)
        );
        undo_conf(&conf, bak.as_deref()).expect("undo");
        assert!(
            fs::read_to_string(&conf).expect("read").contains("t1"),
            "the old one is back"
        );
        let leftovers = fs::read_dir(&dir).expect("dir").count();
        assert_eq!(leftovers, 1, "no temporary file is left");
        let _ = fs::remove_dir_all(&dir);
    }
}
