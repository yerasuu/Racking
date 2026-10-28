//! `rack link` and `rack join` as two real processes against an in-process server:
//! the inviter waits, the joiner redeems, both show the same words, and only a
//! confirmation lets the new agent talk.
#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

const LINUX: &str = "linux-token-0123456789abcdef";
const WINDOWS: &str = "windows-token-0123456789abcdef";

struct Env {
    root: PathBuf,
    url: String,
    server: Option<racking_server::Running>,
}

impl Env {
    fn new() -> Self {
        Self::new_named("ok")
    }

    fn new_named(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("rack-link-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        let config = racking_server::Config::parse(&format!(
            "bind = \"127.0.0.1:0\"\ndata_dir = {:?}\n[agents.linux]\ntoken = \"{LINUX}\"\nrole = \"main\"\n\
             admin = true\n[agents.windows]\ntoken = \"{WINDOWS}\"\n",
            root.join("data").display().to_string()
        ))
        .expect("config");
        let server = racking_server::start(&config).expect("start");
        let url = format!("http://{}", server.addr());
        Self {
            root,
            url,
            server: Some(server),
        }
    }

    /// `rack` with no credentials at all but what the caller adds.
    fn bare(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rack"));
        cmd.args(args)
            .env_remove("RACK_URL")
            .env_remove("RACK_TOKEN")
            .env_remove("RACK_CONFIG")
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("home/.config"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// `rack` as the admin agent `linux`.
    fn admin(&self, args: &[&str]) -> Output {
        self.bare(args)
            .env("RACK_URL", &self.url)
            .env("RACK_TOKEN", LINUX)
            .output()
            .expect("rack")
    }

    /// `rack` with the credentials a join saved in `dir`.
    fn joined(&self, dir: &Path, args: &[&str]) -> Output {
        self.bare(args)
            .env("RACK_CONFIG", dir.join("rack.conf"))
            .output()
            .expect("rack")
    }

    /// Starts `rack join` and reads its output up to the fingerprint, which it prints before waiting.
    fn start_join(&self, code: &str, dir: &Path) -> (Child, String) {
        let child = self
            .bare(&[
                "join",
                code,
                "--server",
                &self.url,
                "--config-dir",
                &dir.display().to_string(),
            ])
            .spawn()
            .expect("rack join");
        wait_for_fingerprint(child)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(s) = self.server.take() {
            s.stop();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The code and invite id from `rack link ... --no-wait`.
fn invite(env: &Env, args: &[&str]) -> (String, String) {
    let out = env.admin(args);
    let all = text(&out);
    assert!(out.status.success(), "{all}");
    let code = all
        .lines()
        .find_map(|l| l.strip_prefix("code: "))
        .unwrap_or_default()
        .to_owned();
    let id = all.split_whitespace().nth(1).unwrap_or_default().to_owned();
    assert_eq!(code.len(), 19, "{all}");
    (code, id)
}

#[test]
fn link_and_join_from_two_terminals() {
    let env = Env::new();
    let mac = env.root.join("mac");

    let (code, id) = invite(&env, &["link", "mac", "--role", "worker", "--no-wait"]);
    let listed = text(&env.admin(&["link", "--list"]));
    assert!(
        listed.contains("mac worker open") && !listed.contains(&code),
        "{listed}"
    );

    let (join, fingerprint) = env.start_join(&code, &mac);
    assert_eq!(fingerprint.split(' ').count(), 4);
    let pending = env.joined(&mac, &["inbox"]);
    assert!(text(&pending).contains("not_confirmed"), "{}", text(&pending));

    let confirm = env.admin(&["link", "--resume", &id, "--yes-if-fingerprint", &fingerprint]);
    let said = text(&confirm);
    assert!(confirm.status.success(), "{said}");
    assert!(said.contains(&fingerprint), "the inviter shows the same words: {said}");
    assert!(said.contains("confirmed: linked with mac"), "{said}");

    let joined = join.wait_with_output().expect("join ends");
    let joined_text = text(&joined);
    assert!(joined.status.success(), "{joined_text}");
    assert!(joined_text.contains("linked with linux"), "{joined_text}");
    assert!(
        joined_text.contains("claude mcp add") && joined_text.contains("rack guide"),
        "{joined_text}"
    );
    assert!(joined_text.contains("crossSessionInbound"), "a worker gets the note");

    let conf = fs::read_to_string(mac.join("rack.conf")).expect("rack.conf");
    let token = conf
        .lines()
        .find_map(|l| l.strip_prefix("token = \""))
        .map(|t| t.trim_end_matches('"'))
        .unwrap_or_default()
        .to_owned();
    assert_eq!(token.len(), 64);
    assert!(!joined_text.contains(&token), "the token is never printed");
    assert!(!said.contains(&token));
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(mac.join("rack.conf")).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let hello = env.joined(&mac, &["say", "linux", "hello from mac"]);
    assert!(hello.status.success(), "{}", text(&hello));
    let inbox = text(&env.admin(&["inbox"]));
    assert!(inbox.contains("hello from mac"), "{inbox}");
    let back = env.admin(&["say", "mac", "welcome"]);
    assert!(back.status.success(), "{}", text(&back));
    let links = text(&env.joined(&mac, &["links"]));
    assert!(links.contains("linux main, via invite, confirmed"), "{links}");
    assert!(links.contains("windows worker, via config"), "{links}");

    let again = env
        .bare(&[
            "join",
            &code,
            "--server",
            &env.url,
            "--config-dir",
            &mac.display().to_string(),
        ])
        .output()
        .expect("rack join");
    assert_eq!(again.status.code(), Some(2), "rack.conf exists: {}", text(&again));

    let gone = env.admin(&["unlink", "mac"]);
    assert!(text(&gone).contains("its token no longer works"), "{}", text(&gone));
    let dead = env.joined(&mac, &["whoami"]);
    assert!(!dead.status.success() && text(&dead).contains("401"), "{}", text(&dead));
}

#[test]
fn denied_or_refused_joins_leave_nothing_usable() {
    let env = Env::new_named("deny");
    let pc = env.root.join("pc");
    let (code, id) = invite(&env, &["link", "--role", "worker", "--no-wait"]);

    fs::create_dir_all(&pc).expect("dir");
    fs::write(pc.join("rack.conf"), "url = \"http://old\"\n").expect("old conf");
    let refused = env
        .bare(&[
            "join",
            &code,
            "--server",
            &env.url,
            "--config-dir",
            &pc.display().to_string(),
        ])
        .output()
        .expect("rack join");
    assert_eq!(refused.status.code(), Some(2), "{}", text(&refused));
    assert!(
        text(&env.admin(&["link", "--list"])).contains(" open"),
        "the code was not used"
    );

    let (join, _) = {
        let mut cmd = env.bare(&[
            "join",
            &code,
            "--server",
            &env.url,
            "--config-dir",
            &pc.display().to_string(),
            "--force",
            "--name",
            "pc",
        ]);
        let child = cmd.spawn().expect("rack join");
        wait_for_fingerprint(child)
    };
    let deny = env.admin(&["link", "--resume", &id, "--yes-if-fingerprint", "x x x x"]);
    assert_eq!(deny.status.code(), Some(1), "{}", text(&deny));
    assert!(text(&deny).contains("denied: pc"), "{}", text(&deny));
    let ended = join.wait_with_output().expect("join ends");
    assert_eq!(ended.status.code(), Some(1), "{}", text(&ended));
    assert!(text(&ended).contains("denied"));
    assert_eq!(
        fs::read_to_string(pc.join("rack.conf")).expect("restored"),
        "url = \"http://old\"\n",
        "the old rack.conf is back"
    );

    let (_, id2) = invite(&env, &["link", "--no-wait"]);
    let cancel = env.admin(&["link", "--cancel", &id2]);
    assert!(cancel.status.success(), "{}", text(&cancel));
    let bad = env
        .bare(&[
            "join",
            "2222-3333-4444-5555",
            "--server",
            &env.url,
            "--config-dir",
            &env.root.join("x").display().to_string(),
        ])
        .output()
        .expect("rack join");
    assert_eq!(bad.status.code(), Some(1));
    assert!(text(&bad).contains("bad_code"));
    let https = env
        .bare(&["join", "2222-3333-4444-5555", "--server", "https://127.0.0.1:1"])
        .output()
        .expect("rack");
    assert_eq!(https.status.code(), Some(2));
}

/// Reads `rack join`'s output up to the fingerprint, which it prints before it waits.
fn wait_for_fingerprint(mut child: Child) -> (Child, String) {
    let mut out = BufReader::new(child.stdout.take().expect("stdout"));
    let mut seen = String::new();
    let mut line = String::new();
    let mut fingerprint = String::new();
    while out.read_line(&mut line).expect("read") > 0 {
        seen.push_str(&line);
        if let Some(words) = line.strip_prefix("fingerprint: ") {
            fingerprint = words.trim().to_owned();
            break;
        }
        line.clear();
    }
    assert!(!fingerprint.is_empty(), "rack join printed no fingerprint: {seen}");
    // The rest is read once the process ends.
    child.stdout = Some(out.into_inner());
    (child, fingerprint)
}
