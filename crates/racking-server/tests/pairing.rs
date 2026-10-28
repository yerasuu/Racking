//! Pairing (#46): invites, `POST /join`, the pending state, confirmation and the
//! links that decide who may send, against a real server on a random loopback port.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BOSS: &str = "boss-token-0123456789abcdef";
const W1: &str = "w1-token-0123456789abcdef0";

/// `boss` is main and admin, `w1` a worker; both in the config file.
const ROLES: &str = "[agents.boss]\ntoken = \"boss-token-0123456789abcdef\"\nrole = \"main\"\nadmin = true\n\
                     [agents.w1]\ntoken = \"w1-token-0123456789abcdef0\"\n";

fn temp_dir(tag: &str) -> PathBuf {
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    let run = RUNS.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("racking-pair-{tag}-{}-{run}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .new_agent()
}

fn call(http: &ureq::Agent, base: &str, method: &str, path: &str, token: &str, body: Option<Value>) -> (u16, Value) {
    let url = format!("{base}{path}");
    let auth = format!("Bearer {token}");
    let resp = match (method, body) {
        ("GET", _) => http.get(&url).header("Authorization", &auth).call(),
        ("DELETE", _) => http.delete(&url).header("Authorization", &auth).call(),
        (_, Some(b)) => http.post(&url).header("Authorization", &auth).send_json(b),
        (_, None) => http.post(&url).header("Authorization", &auth).send_empty(),
    };
    let mut resp = resp.expect("http");
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().expect("body");
    (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

struct Server {
    running: Option<racking_server::Running>,
    base: String,
    dir: PathBuf,
    http: ureq::Agent,
    clock: racking_server::Clock,
}

impl Server {
    fn start(extra: &str) -> Self {
        let clock = racking_server::Clock::manual(1_800_000_000);
        let dir = temp_dir("srv");
        let config = racking_server::Config::parse(&format!(
            "bind = \"127.0.0.1:0\"\ndata_dir = {:?}\n{extra}",
            dir.display().to_string()
        ))
        .expect("config");
        let running = racking_server::start_with_clock(&config, clock.clone()).expect("start");
        let base = format!("http://{}", running.addr());
        Self {
            running: Some(running),
            base,
            dir,
            http: http(),
            clock,
        }
    }

    fn call(&self, method: &str, path: &str, token: &str, body: Option<Value>) -> (u16, Value) {
        call(&self.http, &self.base, method, path, token, body)
    }

    /// `POST /join` without a token.
    fn join(&self, body: Value) -> (u16, Value) {
        let mut resp = self
            .http
            .post(format!("{}/join", self.base))
            .send_json(body)
            .expect("http");
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().expect("body");
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    fn invite(&self, body: Value) -> (i64, String) {
        let (status, made) = self.call("POST", "/admin/invites", BOSS, Some(body));
        assert_eq!(status, 201, "{made}");
        (
            made["id"].as_i64().unwrap_or_default(),
            made["code"].as_str().unwrap_or_default().to_owned(),
        )
    }

    /// Invites, joins and confirms an agent; returns its token.
    fn linked(&self, name: &str, role: &str) -> String {
        let (id, code) = self.invite(json!({"name": name, "role": role}));
        let (status, joined) = self.join(json!({ "code": code }));
        assert_eq!(status, 201, "{joined}");
        assert_eq!(
            self.call("POST", &format!("/admin/invites/{id}/confirm"), BOSS, None).0,
            200
        );
        joined["token"].as_str().unwrap_or_default().to_owned()
    }

    fn database_bytes(&self) -> Vec<u8> {
        ["racking.db", "racking.db-wal"]
            .iter()
            .filter_map(|f| std::fs::read(self.dir.join(f)).ok())
            .flatten()
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(r) = self.running.take() {
            r.stop();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or_default()
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[test]
fn join_waits_for_the_inviter_then_both_can_talk() {
    let s = Server::start(ROLES);
    let (status, made) = s.call(
        "POST",
        "/admin/invites",
        BOSS,
        Some(json!({"name": "mac", "role": "worker"})),
    );
    assert_eq!(status, 201, "{made}");
    let id = made["id"].as_i64().unwrap_or_default();
    let code_text = made["code"].as_str().unwrap_or_default().to_owned();
    assert_eq!(code_text.len(), 19);

    let (_, list) = s.call("GET", "/admin/invites", BOSS, None);
    assert_eq!(list["invites"][0]["state"], "open");
    assert!(!list.to_string().contains(&code_text), "no code is listed");

    let (status, joined) = s.join(json!({"code": code_text.to_lowercase(), "os": "linux", "rack_version": "0.1.3"}));
    assert_eq!(status, 201, "{joined}");
    let token = joined["token"].as_str().unwrap_or_default().to_owned();
    let fingerprint = joined["fingerprint"].as_str().unwrap_or_default().to_owned();
    assert_eq!(fingerprint.split(' ').count(), 4);
    assert_eq!(
        (joined["name"].as_str(), joined["inviter"].as_str()),
        (Some("mac"), Some("boss"))
    );

    let (_, seen) = s.call("GET", &format!("/admin/invites/{id}"), BOSS, None);
    assert_eq!(seen["state"], "redeemed");
    assert_eq!(
        seen["fingerprint"],
        fingerprint.as_str(),
        "the inviter sees the same words"
    );
    assert_eq!(
        (seen["os"].as_str(), seen["peer"].as_str()),
        (Some("linux"), Some("127.0.0.1"))
    );

    for path in ["/inbox", "/whoami", "/agent-links"] {
        let (status, err) = s.call("GET", path, &token, None);
        assert_eq!((status, code(&err)), (403, "not_confirmed"), "{path}");
        assert!(
            err["error"]["hint"].as_str().unwrap_or_default().contains(&fingerprint),
            "{err}"
        );
    }
    let (status, err) = s.call(
        "POST",
        "/jobs",
        BOSS,
        Some(json!({"to": "mac", "kind": "message", "body": "x"})),
    );
    assert_eq!(
        (status, code(&err)),
        (403, "not_allowed"),
        "nothing reaches a pending agent"
    );
    let (_, st) = s.call("GET", "/join/status", &token, None);
    assert_eq!(st["state"], "pending");

    assert_eq!(s.call("POST", &format!("/admin/invites/{id}/confirm"), W1, None).0, 403);
    let (status, done) = s.call("POST", &format!("/admin/invites/{id}/confirm"), BOSS, None);
    assert_eq!((status, done["state"].as_str()), (200, Some("confirmed")), "{done}");
    let (_, st) = s.call("GET", "/join/status?timeout=1", &token, None);
    assert_eq!(
        (st["state"].as_str(), st["inviter"].as_str()),
        (Some("confirmed"), Some("boss"))
    );

    let (status, sent) = s.call(
        "POST",
        "/jobs",
        &token,
        Some(json!({"to": "boss", "kind": "message", "body": "hello"})),
    );
    assert_eq!(status, 201, "{sent}");
    let (status, _) = s.call(
        "POST",
        "/jobs",
        BOSS,
        Some(json!({"to": "mac", "title": "Build", "body": "go"})),
    );
    assert_eq!(status, 201);
    let (_, inbox) = s.call("GET", "/inbox", &token, None);
    assert_eq!(inbox["jobs"][0]["from"], "boss");

    let (_, links) = s.call("GET", "/agent-links", &token, None);
    assert_eq!(links["source"], "api");
    let with: Vec<(&str, &str)> = links["links"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|l| (l["agent"].as_str().unwrap_or(""), l["via"].as_str().unwrap_or("")))
        .collect();
    assert!(with.contains(&("boss", "invite")), "{with:?}");
    assert!(
        with.contains(&("w1", "config")),
        "config agents are linked with everyone"
    );
    let (_, boss_links) = s.call("GET", "/agent-links", BOSS, None);
    assert!(boss_links.to_string().contains("\"mac\""));

    let (_, audit) = s.call("GET", "/admin/audit?limit=100", BOSS, None);
    let text = audit.to_string();
    for action in ["invite.create", "join", "invite.confirm"] {
        assert!(text.contains(action), "{action} in {text}");
    }
    let bytes = s.database_bytes();
    let upper = code_text.replace('-', "");
    for secret in [code_text.as_str(), upper.as_str(), token.as_str()] {
        assert!(!text.contains(secret), "the audit holds no code or token");
        assert!(!contains(&bytes, secret), "the database holds no code or token");
    }
}

#[test]
fn deny_kills_the_token_and_a_code_works_once() {
    let s = Server::start(ROLES);
    let (id, code_text) = s.invite(json!({"role": "worker"}));
    let (status, joined) = s.join(json!({ "code": code_text }));
    assert_eq!(status, 201);
    let name = joined["name"].as_str().unwrap_or_default().to_owned();
    assert!(name.starts_with("agent-"), "the server proposed {name}");
    let token = joined["token"].as_str().unwrap_or_default().to_owned();

    let (status, again) = s.join(json!({ "code": code_text }));
    assert_eq!((status, code(&again)), (403, "bad_code"), "single use");

    assert_eq!(s.call("POST", &format!("/admin/invites/{id}/deny"), BOSS, None).0, 200);
    let (status, err) = s.call("GET", "/inbox", &token, None);
    assert_eq!((status, code(&err)), (401, "bad_token"), "the token is dead");
    let (_, st) = s.call("GET", "/join/status", &token, None);
    assert_eq!(st["state"], "denied");
    let (_, agents) = s.call("GET", "/admin/agents", BOSS, None);
    assert!(!agents.to_string().contains(&name), "the pending agent is gone");
    assert_eq!(
        s.call("POST", &format!("/admin/invites/{id}/confirm"), BOSS, None).0,
        409,
        "decided once"
    );
}

#[test]
fn five_wrong_codes_void_the_invite_and_lock_the_peer() {
    let s = Server::start(ROLES);
    let (id, good) = s.invite(json!({"name": "mac", "role": "worker"}));
    for bad in [
        "2222-2222-2222-2222",
        "not a code",
        "3333-3333-3333-3333",
        "",
        "4444-4444-4444-4444",
    ] {
        let (status, err) = s.join(json!({ "code": bad }));
        assert_eq!((status, code(&err)), (403, "bad_code"), "{bad}");
    }
    let (_, invite) = s.call("GET", &format!("/admin/invites/{id}"), BOSS, None);
    assert_eq!(
        (invite["state"].as_str(), invite["failures"].as_i64()),
        (Some("voided"), Some(5))
    );
    let (status, err) = s.join(json!({ "code": good }));
    assert_eq!(
        (status, code(&err)),
        (429, "rate_limited"),
        "this address is locked out"
    );
    s.clock.advance(901);
    let (status, err) = s.join(json!({ "code": good }));
    assert_eq!((status, code(&err)), (403, "bad_code"), "a voided code stays dead");
    let (_, audit) = s.call("GET", "/admin/audit?limit=100", BOSS, None);
    assert!(audit.to_string().contains("too_many_failures"));
}

#[test]
fn expired_codes_and_late_confirmations() {
    let s = Server::start(ROLES);
    let (_, old) = s.invite(json!({"name": "mac", "role": "worker", "ttl_secs": 60}));
    s.clock.advance(61);
    let (status, err) = s.join(json!({ "code": old }));
    assert_eq!((status, code(&err)), (403, "bad_code"));

    let (id, code_text) = s.invite(json!({"name": "mac", "role": "worker"}));
    let (_, joined) = s.join(json!({ "code": code_text }));
    let token = joined["token"].as_str().unwrap_or_default().to_owned();
    s.clock.advance(601);
    let (status, _) = s.call("POST", &format!("/admin/invites/{id}/confirm"), BOSS, None);
    assert_eq!(status, 409, "too late to confirm");
    let (_, st) = s.call("GET", "/join/status", &token, None);
    assert_eq!(st["state"], "denied");
    assert_eq!(s.call("GET", "/whoami", &token, None).0, 401);
}

#[test]
fn invite_rules_names_and_admin_only() {
    let s = Server::start(ROLES);
    let (status, err) = s.call("POST", "/admin/invites", W1, Some(json!({"role": "worker"})));
    assert_eq!((status, code(&err)), (403, "not_admin"));
    let (status, _) = s.call("POST", "/admin/invites", BOSS, Some(json!({"role": "main"})));
    assert_eq!(status, 409, "boss is main already");
    let (status, _) = s.call(
        "POST",
        "/admin/invites",
        BOSS,
        Some(json!({"role": "worker", "name": "w1"})),
    );
    assert_eq!(status, 409, "w1 exists");
    let (status, _) = s.call(
        "POST",
        "/admin/invites",
        BOSS,
        Some(json!({"role": "worker", "ttl_secs": 3601})),
    );
    assert_eq!(status, 400);
    let (id, code_text) = s.invite(json!({"name": "mac", "role": "worker"}));
    let (status, _) = s.call(
        "POST",
        "/admin/invites",
        BOSS,
        Some(json!({"role": "worker", "name": "mac"})),
    );
    assert_eq!(status, 409, "an open invite holds the name");
    let (status, _) = s.call(
        "POST",
        "/admin/agents",
        BOSS,
        Some(json!({"name": "mac", "role": "worker"})),
    );
    assert_eq!(status, 409, "the admin API respects it too");
    let (status, err) = s.join(json!({"code": code_text, "name": "other"}));
    assert_eq!((status, code(&err)), (409, "conflict"), "the invite names its agent");

    assert_eq!(s.call("DELETE", &format!("/admin/invites/{id}"), BOSS, None).0, 200);
    let (status, err) = s.join(json!({ "code": code_text }));
    assert_eq!((status, code(&err)), (403, "bad_code"), "a cancelled code is dead");
    let (_, list) = s.call("GET", "/admin/invites", BOSS, None);
    assert_eq!(list["invites"], json!([]));
}

#[test]
fn links_decide_who_api_agents_reach() {
    let config = "[agents.boss]\ntoken = \"boss-token-0123456789abcdef\"\nadmin = true\n\
                  [agents.w1]\ntoken = \"w1-token-0123456789abcdef0\"\n\
                  [permissions]\nsend = { mac = [\"pc\"], pc = [\"mac\"] }\n";
    let s = Server::start(config);
    let chief = s.linked("chief", "main");
    let mac = s.linked("mac", "worker");
    let pc = s.linked("pc", "worker");

    let (status, err) = s.call(
        "POST",
        "/jobs",
        &chief,
        Some(json!({"to": "mac", "kind": "message", "body": "x"})),
    );
    assert_eq!(
        (status, code(&err)),
        (403, "not_allowed"),
        "two API agents without a link"
    );
    let (status, _) = s.call("POST", "/admin/links", BOSS, Some(json!({"a": "chief", "b": "mac"})));
    assert_eq!(status, 409, "main links only through invites");
    let (status, _) = s.call(
        "POST",
        "/jobs",
        &chief,
        Some(json!({"to": "w1", "kind": "message", "body": "x"})),
    );
    assert_eq!(status, 201, "a config agent is linked with everyone");

    let (status, _) = s.call(
        "POST",
        "/jobs",
        &mac,
        Some(json!({"to": "pc", "kind": "message", "body": "x"})),
    );
    assert_eq!(status, 403, "permitted by [permissions], but not linked");
    assert_eq!(
        s.call("POST", "/admin/links", W1, Some(json!({"a": "mac", "b": "pc"})))
            .0,
        403
    );
    let (status, _) = s.call("POST", "/admin/links", BOSS, Some(json!({"a": "mac", "b": "pc"})));
    assert_eq!(status, 201);
    let (status, sent) = s.call(
        "POST",
        "/jobs",
        &mac,
        Some(json!({"to": "pc", "kind": "message", "body": "hi"})),
    );
    assert_eq!(status, 201, "{sent}");
    let (_, me) = s.call("GET", "/whoami", &mac, None);
    assert!(me["peers"].as_array().is_some_and(|p| p.contains(&json!("pc"))), "{me}");

    let (status, gone) = s.call("DELETE", "/admin/links/mac/pc", BOSS, None);
    assert_eq!(
        (status, gone["revoked"].clone()),
        (200, json!([])),
        "an admin link revokes nobody"
    );
    let (status, _) = s.call(
        "POST",
        "/jobs",
        &mac,
        Some(json!({"to": "pc", "kind": "message", "body": "x"})),
    );
    assert_eq!(status, 403);

    let (status, gone) = s.call("DELETE", "/admin/links/boss/mac", BOSS, None);
    assert_eq!((status, gone["revoked"].clone()), (200, json!(["mac"])), "{gone}");
    assert_eq!(
        s.call("GET", "/whoami", &mac, None).0,
        401,
        "unlinked from its inviter, its token is dead"
    );
    let id = sent["id"].as_str().unwrap_or_default();
    assert_eq!(
        s.call("GET", &format!("/jobs/{id}"), &pc, None).0,
        200,
        "its jobs are kept"
    );
    assert_eq!(
        s.call("DELETE", "/admin/links/boss/w1", BOSS, None).0,
        404,
        "config agents have no stored link to remove"
    );
    let (_, audit) = s.call("GET", "/admin/audit?limit=100", BOSS, None);
    let text = audit.to_string();
    assert!(text.contains("link.add") && text.contains("link.remove"), "{text}");
}

#[test]
fn allow_from_applies_to_join() {
    let s = Server::start(&format!("allow_from = [\"10.0.0.0/8\"]\n{ROLES}"));
    let (status, err) = s.join(json!({"code": "2222-2222-2222-2222"}));
    assert_eq!((status, code(&err)), (403, "peer_not_allowed"));
    let (status, err) = s.call("GET", "/join/status", "anything", None);
    assert_eq!((status, code(&err)), (403, "peer_not_allowed"));
}

/// `POST /join` as if a reverse proxy on loopback forwarded it for `client`.
fn join_for(s: &Server, client: &str, code: &str) -> (u16, Value) {
    let mut resp = s
        .http
        .post(format!("{}/join", s.base))
        .header("X-Forwarded-For", client)
        .send_json(json!({ "code": code }))
        .expect("http");
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().expect("body");
    (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

#[test]
fn a_trusted_proxy_s_client_is_counted_and_audited_by_its_own_address() {
    let s = Server::start(&format!(
        "allow_from = [\"0.0.0.0/0\"]\ntrusted_proxies = [\"127.0.0.0/8\"]\n{ROLES}"
    ));
    s.invite(json!({"name": "mac", "role": "worker"}));
    for _ in 0..5 {
        let (status, err) = join_for(&s, "1.2.3.4, 203.0.113.9", "2222-2222-2222-2222");
        assert_eq!((status, code(&err)), (403, "bad_code"));
    }
    let (_, good) = s.invite(json!({"name": "pc", "role": "worker"}));
    let (status, err) = join_for(&s, "203.0.113.9", &good);
    assert_eq!((status, code(&err)), (429, "rate_limited"), "the guesser is locked out");
    let (status, err) = join_for(&s, "1.2.3.4, 203.0.113.9", &good);
    assert_eq!(
        (status, code(&err)),
        (429, "rate_limited"),
        "a spoofed leftmost entry changes nothing"
    );
    let (status, joined) = join_for(&s, "203.0.113.10", &good);
    assert_eq!(status, 201, "another client behind the same proxy is not: {joined}");
    let (_, audit) = s.call("GET", "/admin/audit?limit=100", BOSS, None);
    let text = audit.to_string();
    assert!(text.contains("203.0.113.9") && !text.contains("1.2.3.4"), "{text}");

    let lan_only = Server::start(&format!(
        "allow_from = [\"127.0.0.0/8\"]\ntrusted_proxies = [\"127.0.0.0/8\"]\n{ROLES}"
    ));
    let (status, err) = join_for(&lan_only, "203.0.113.9", "2222-2222-2222-2222");
    assert_eq!(
        (status, code(&err)),
        (403, "peer_not_allowed"),
        "allow_from sees the client"
    );
}

#[test]
fn forwarded_for_is_ignored_without_trusted_proxies() {
    let s = Server::start(ROLES);
    s.invite(json!({"name": "mac", "role": "worker"}));
    for _ in 0..5 {
        assert_eq!(join_for(&s, "203.0.113.9", "2222-2222-2222-2222").0, 403);
    }
    let (_, good) = s.invite(json!({"name": "pc", "role": "worker"}));
    let (status, err) = join_for(&s, "203.0.113.10", &good);
    assert_eq!(
        (status, code(&err)),
        (429, "rate_limited"),
        "the TCP peer is what was counted"
    );
    let (_, audit) = s.call("GET", "/admin/audit?limit=100", BOSS, None);
    assert!(!audit.to_string().contains("203.0.113"), "{audit}");
}

/// The real server binary, so its log can be searched for secrets.
struct Process {
    child: Child,
    base: String,
    dir: PathBuf,
    config: PathBuf,
    log: Option<JoinHandle<String>>,
}

impl Process {
    fn start() -> Self {
        let dir = temp_dir("proc");
        std::fs::create_dir_all(&dir).expect("dir");
        let config = dir.join("server.toml");
        std::fs::write(
            &config,
            format!(
                "bind = \"127.0.0.1:0\"\ndata_dir = {:?}\n{ROLES}",
                dir.join("data").display().to_string()
            ),
        )
        .expect("config");
        let mut child = Command::new(env!("CARGO_BIN_EXE_racking-server"))
            .args(["--config", &config.display().to_string()])
            .stderr(Stdio::piped())
            .spawn()
            .expect("server");
        let mut lines = BufReader::new(child.stderr.take().expect("stderr"));
        let mut base = String::new();
        let mut line = String::new();
        while lines.read_line(&mut line).expect("log") > 0 {
            if let Some(addr) = line.split("addr=").nth(1) {
                base = format!("http://{}", addr.split_whitespace().next().unwrap_or_default());
                break;
            }
            line.clear();
        }
        assert!(!base.is_empty(), "the server did not say where it listens");
        // Keep reading the log in the background so the server never blocks on a full pipe;
        // the thread ends at EOF, when the server has exited, and hands back what it read.
        let log = std::thread::spawn(move || {
            let mut rest = String::new();
            let _ = lines.read_to_string(&mut rest);
            rest
        });
        Self {
            child,
            base,
            dir,
            config,
            log: Some(log),
        }
    }

    fn invite(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_racking-server"))
            .args(["invite", "--config", &self.config.display().to_string()])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("racking-server invite")
    }

    fn join(&self, code: &str) -> Value {
        let mut resp = http()
            .post(format!("{}/join", self.base))
            .send_json(json!({ "code": code }))
            .expect("http");
        assert_eq!(resp.status().as_u16(), 201);
        resp.body_mut().read_json().expect("json")
    }

    /// Stops the server as Docker and systemd do (SIGTERM), waits for it to exit and returns
    /// everything it logged. The log is complete only once the pipe is at EOF.
    fn stop(mut self) -> String {
        let status = Command::new("kill")
            .args(["-TERM", "--", &self.child.id().to_string()])
            .status()
            .expect("kill");
        assert!(status.success(), "kill -TERM failed");
        let started = Instant::now();
        while self.child.try_wait().expect("try_wait").is_none() {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the server did not exit on SIGTERM"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        self.log.take().expect("log reader").join().expect("log thread")
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn field<'a>(text: &'a str, prefix: &str) -> &'a str {
    text.lines().find_map(|l| l.strip_prefix(prefix)).map_or("", str::trim)
}

#[test]
fn owner_bootstrap_invite_and_nothing_secret_in_the_log() {
    let server = Process::start();
    let made = server.invite(&["--name", "mac", "--no-wait"]);
    let out = String::from_utf8_lossy(&made.stdout).into_owned();
    assert!(made.status.success(), "{out}{}", String::from_utf8_lossy(&made.stderr));
    let code_text = field(&out, "code: ").to_owned();
    let id = out.split_whitespace().nth(1).unwrap_or_default().to_owned();
    assert_eq!(code_text.len(), 19, "{out}");

    let joined = server.join(&code_text);
    let token = joined["token"].as_str().unwrap_or_default().to_owned();
    let fingerprint = joined["fingerprint"].as_str().unwrap_or_default().to_owned();
    assert_eq!(joined["inviter"], Value::Null, "the owner is no agent");
    let http = http();
    let (status, err) = call(&http, &server.base, "GET", "/whoami", &token, None);
    assert_eq!((status, code(&err)), (403, "not_confirmed"));

    // Never a fingerprint: the words are four, and `x` is none of them.
    let wrong = server.invite(&["--resume", &id, "--yes-if-fingerprint", "x x x x"]);
    assert_eq!(wrong.status.code(), Some(1), "other words: denied");
    let reply = String::from_utf8_lossy(&wrong.stdout).into_owned();
    assert!(reply.contains(&fingerprint), "the owner sees the same words: {reply}");
    let (status, me) = call(&http, &server.base, "GET", "/whoami", &token, None);
    assert_eq!(status, 401, "the denied token is dead: {me}");

    // A second joiner, confirmed this time with the words its own screen showed.
    let made = server.invite(&["--name", "pc", "--no-wait"]);
    let out = String::from_utf8_lossy(&made.stdout).into_owned();
    let code2 = field(&out, "code: ").to_owned();
    let id2 = out.split_whitespace().nth(1).unwrap_or_default().to_owned();
    let joined = server.join(&code2);
    let token2 = joined["token"].as_str().unwrap_or_default().to_owned();
    let words = joined["fingerprint"].as_str().unwrap_or_default().replace(' ', "-");
    let ok = server.invite(&["--resume", &id2, "--yes-if-fingerprint", &words]);
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stdout));
    let (status, me) = call(&http, &server.base, "GET", "/whoami", &token2, None);
    assert_eq!(
        (status, me["agent"].as_str()),
        (200, Some("pc")),
        "the running server picked it up"
    );
    let (status, _) = call(
        &http,
        &server.base,
        "POST",
        "/jobs",
        &token2,
        Some(json!({"to": "boss", "kind": "message", "body": "hello"})),
    );
    assert_eq!(status, 201);

    let data = server.dir.join("data");
    let log = server.stop();
    assert!(log.contains("racking::pairing") || log.contains("/join"), "{log}");
    let db: Vec<u8> = ["racking.db", "racking.db-wal"]
        .iter()
        .filter_map(|f| std::fs::read(Path::new(&data).join(f)).ok())
        .flatten()
        .collect();
    for secret in [&code_text, &code2, &token, &token2] {
        assert!(!log.contains(secret.as_str()), "a code or token is in the server log");
        assert!(!contains(&db, secret), "a code or token is in the database");
    }
}
