//! Broadcasts (#28): one notice, message or report to a list of agents or to `"*"`,
//! against a real server on a random loopback port.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const BOSS: &str = "boss-token-0123456789abcdef";
const W1: &str = "w1-token-0123456789abcdef0";
const W2: &str = "w2-token-0123456789abcdef0";

/// `boss` is main and admin, `w1` and `w2` are workers of the config file.
const ROLES: &str = "[agents.boss]\ntoken = \"boss-token-0123456789abcdef\"\nrole = \"main\"\nadmin = true\n\
                     [agents.w1]\ntoken = \"w1-token-0123456789abcdef0\"\n\
                     [agents.w2]\ntoken = \"w2-token-0123456789abcdef0\"\n";

struct Server {
    running: Option<racking_server::Running>,
    base: String,
    dir: PathBuf,
    http: ureq::Agent,
}

impl Server {
    fn start(extra: &str, clock: racking_server::Clock) -> Self {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("racking-bcast-{}-{run}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = racking_server::Config::parse(&format!(
            "bind = \"127.0.0.1:0\"\ndata_dir = {:?}\n{extra}",
            dir.display().to_string()
        ))
        .expect("config");
        let running = racking_server::start_with_clock(&config, clock).expect("start");
        let base = format!("http://{}", running.addr());
        let http = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .new_agent();
        Self {
            running: Some(running),
            base,
            dir,
            http,
        }
    }

    fn answer(resp: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> (u16, Value) {
        let mut resp = resp.expect("http");
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().expect("body");
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    fn get(&self, path: &str, token: &str) -> (u16, Value) {
        Self::answer(
            self.http
                .get(format!("{}{path}", self.base))
                .header("Authorization", format!("Bearer {token}"))
                .call(),
        )
    }

    fn post(&self, path: &str, token: &str, body: Option<Value>) -> (u16, Value) {
        let req = self
            .http
            .post(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {token}"));
        Self::answer(match body {
            Some(b) => req.send_json(b),
            None => req.send_empty(),
        })
    }

    fn send(&self, token: &str, job: Value) -> (u16, Value) {
        self.post("/jobs", token, Some(job))
    }

    fn send_keyed(&self, token: &str, key: &str, job: &Value) -> (u16, Value) {
        Self::answer(
            self.http
                .post(format!("{}/jobs", self.base))
                .header("Authorization", format!("Bearer {token}"))
                .header("Idempotency-Key", key)
                .send_json(job),
        )
    }

    fn attach(&self, token: &str, job: &str, name: &str, bytes: &[u8]) -> u16 {
        Self::answer(
            self.http
                .put(format!("{}/jobs/{job}/attachments/{name}", self.base))
                .header("Authorization", format!("Bearer {token}"))
                .header("X-Sha256", sha(bytes))
                .send(bytes),
        )
        .0
    }

    /// Invites `name` as a worker and joins it; confirmed or left pending. Returns its token.
    fn joined(&self, name: &str, confirm: bool) -> String {
        let (status, made) = self.post("/admin/invites", BOSS, Some(json!({"name": name, "role": "worker"})));
        assert_eq!(status, 201, "{made}");
        let (status, joined) = Self::answer(
            self.http
                .post(format!("{}/join", self.base))
                .send_json(json!({"code": made["code"]})),
        );
        assert_eq!(status, 201, "{joined}");
        if confirm {
            let id = made["id"].as_i64().unwrap_or_default();
            assert_eq!(self.post(&format!("/admin/invites/{id}/confirm"), BOSS, None).0, 200);
        }
        joined["token"].as_str().unwrap_or_default().to_owned()
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

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or_default()
}

/// `(to, id)` of each job a broadcast made.
fn made(answer: &Value) -> Vec<(String, String)> {
    answer["jobs"]
        .as_array()
        .map(|jobs| {
            jobs.iter()
                .map(|j| {
                    (
                        j["to"].as_str().unwrap_or_default().to_owned(),
                        j["id"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn names(answer: &Value) -> Vec<String> {
    made(answer).into_iter().map(|(to, _)| to).collect()
}

#[test]
fn lists_and_everyone_follow_the_send_rules() {
    let s = Server::start(ROLES, racking_server::Clock::system());
    let mac = s.joined("mac", true);
    let _pending = s.joined("pc", false);

    let (status, sent) = s.send(
        BOSS,
        json!({"to": ["w1", "w2", "w1"], "kind": "message", "body": "standup at 10", "expects_reply": true}),
    );
    assert_eq!(status, 201, "{sent}");
    assert_eq!(names(&sent), ["w1", "w2"], "repeats are sent once");
    assert!(sent["group"].as_str().is_some_and(|g| g.starts_with('g')), "{sent}");
    assert_eq!(
        (sent["skipped"].clone(), sent["skipped_count"].clone()),
        (json!([]), json!(0))
    );
    assert!(
        sent["jobs"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|j| j["state"] == "sent")
    );

    let (status, all) = s.send(BOSS, json!({"to": "*", "kind": "notice", "body": "deploying now"}));
    assert_eq!(status, 201, "{all}");
    assert_eq!(
        names(&all),
        ["w1", "w2", "mac"],
        "never the sender, never a pending agent"
    );
    assert_eq!(
        (all["skipped"].clone(), all["skipped_count"].clone()),
        (json!([]), json!(1)),
        "`*` names nobody it skipped, it only counts"
    );
    assert_ne!(all["group"], sent["group"]);

    // A worker reaches only main: the others are skipped, and an unknown name looks the same.
    let (status, part) = s.send(
        W1,
        json!({"to": ["boss", "w2", "ghost"], "kind": "report", "title": "Probe", "body": "two devices"}),
    );
    assert_eq!(status, 201, "partial success: {part}");
    assert_eq!(names(&part), ["boss"]);
    assert_eq!(
        part["skipped"],
        json!([{"to": "w2", "reason": "not_allowed"}, {"to": "ghost", "reason": "not_allowed"}])
    );
    assert_eq!(part["skipped_count"], 2);
    let (status, none) = s.send(
        W1,
        json!({"to": ["w2", "ghost", "pc"], "kind": "message", "body": "hi"}),
    );
    assert_eq!((status, code(&none)), (403, "not_allowed"), "{none}");
    assert!(
        none["error"]["hint"].as_str().unwrap_or_default().contains("boss"),
        "{none}"
    );
    let (status, star) = s.send(W1, json!({"to": "*", "kind": "notice", "body": "CI red"}));
    assert_eq!((status, names(&star)), (201, vec!["boss".to_owned()]));
    assert_eq!(star["skipped_count"], 3);

    // mac joined through boss's invite: linked with boss only, so w-agents it may not reach.
    let (status, from_mac) = s.send(&mac, json!({"to": "*", "kind": "message", "body": "hello"}));
    assert_eq!((status, names(&from_mac)), (201, vec!["boss".to_owned()]), "{from_mac}");

    // Each recipient has its own job: own id, own seq, own state.
    let (_, inbox1) = s.get("/inbox", W1);
    let (_, inbox2) = s.get("/inbox?peek=1", W2);
    let first = |inbox: &Value| inbox["jobs"][0].clone();
    assert_ne!(first(&inbox1)["id"], first(&inbox2)["id"]);
    assert_eq!(first(&inbox1)["group"], sent["group"]);
    assert_eq!(first(&inbox1)["state"], "received");
    assert_eq!(first(&inbox2)["state"], "sent", "w2 has not read its copy");
    assert_eq!(
        (first(&inbox1)["seq"].clone(), first(&inbox2)["seq"].clone()),
        (json!(1), json!(1))
    );

    // w1 answers its copy; only that copy becomes answered.
    let w1_copy = first(&inbox1)["id"].clone();
    let (status, _) = s.send(
        W1,
        json!({"to": "boss", "kind": "message", "body": "ok", "reply_to": w1_copy}),
    );
    assert_eq!(status, 201);
    let (_, out) = s.get("/outbox?since=0&limit=200", BOSS);
    let jobs = out["jobs"].as_array().cloned().unwrap_or_default();
    let of = |id: &str| jobs.iter().find(|j| j["id"] == id).cloned().unwrap_or_default();
    let copies = made(&sent);
    assert_eq!(of(&copies[0].1)["state"], "answered");
    assert_eq!(of(&copies[1].1)["state"], "sent");
    assert!(
        copies.iter().all(|(_, id)| of(id)["group"] == sent["group"]),
        "the outbox shows the group"
    );
}

#[test]
fn tasks_and_replies_go_to_one_agent() {
    let s = Server::start(ROLES, racking_server::Clock::system());
    for to in [json!(["w1", "w2"]), json!("*"), json!(["w1"])] {
        let (status, err) = s.send(BOSS, json!({"to": to, "title": "Build it", "body": "make"}));
        assert_eq!((status, code(&err)), (400, "bad_request"), "{to}: {err}");
        assert!(
            err["error"]["hint"]
                .as_str()
                .unwrap_or_default()
                .contains("one task per agent")
        );
    }
    let (_, one) = s.send(BOSS, json!({"to": "w1", "kind": "message", "body": "x"}));
    let (status, err) = s.send(
        BOSS,
        json!({"to": ["w1", "w2"], "kind": "message", "body": "x", "reply_to": one["id"]}),
    );
    assert_eq!((status, code(&err)), (400, "bad_request"), "{err}");
    let many: Vec<String> = (0..17).map(|i| format!("a{i}")).collect();
    let (status, _) = s.send(BOSS, json!({"to": many, "kind": "notice", "body": "x"}));
    assert_eq!(status, 400, "at most 16");
    let (status, _) = s.send(BOSS, json!({"to": ["w1", "boss"], "kind": "notice", "body": "x"}));
    assert_eq!(status, 400, "never yourself");
    let (status, err) = s.send(
        BOSS,
        json!({"to": ["w1", "w2"], "kind": "notice", "body": "x".repeat(2000)}),
    );
    assert_eq!(
        (status, code(&err)),
        (413, "too_large"),
        "fields are checked as for one: {err}"
    );
    let (_, out) = s.get("/outbox", BOSS);
    assert_eq!(out["jobs"].as_array().map(Vec::len), Some(1), "nothing else was stored");
}

#[test]
fn a_replay_returns_the_same_answer_and_no_second_copy() {
    let s = Server::start(ROLES, racking_server::Clock::system());
    let job = json!({"to": ["w1", "w2"], "kind": "message", "body": "once"});
    let (status, first) = s.send_keyed(BOSS, "bcast-1", &job);
    assert_eq!(status, 201);
    let (status, again) = s.send_keyed(BOSS, "bcast-1", &job);
    assert_eq!((status, &again), (201, &first), "the stored answer");
    let (_, out) = s.get("/outbox", BOSS);
    assert_eq!(out["jobs"].as_array().map(Vec::len), Some(2), "no duplicates");
    let other = json!({"to": ["w1"], "kind": "message", "body": "once"});
    assert_eq!(s.send_keyed(BOSS, "bcast-1", &other).0, 422, "same key, other request");

    let refused = json!({"to": ["w2"], "kind": "message", "body": "x"});
    let (status, err) = s.send_keyed(W1, "bcast-2", &refused);
    assert_eq!(status, 403);
    assert_eq!(
        s.send_keyed(W1, "bcast-2", &refused),
        (403, err),
        "a refusal replays too"
    );
}

#[test]
fn every_job_counts_against_the_rate_limit() {
    let clock = racking_server::Clock::manual(6_000);
    let s = Server::start(&format!("{ROLES}[limits]\njobs_per_minute = 3\n"), clock.clone());
    let job = || json!({"to": ["w1", "w2"], "kind": "notice", "body": "tick"});
    assert_eq!(s.send(BOSS, job()).0, 201, "two of three");
    let (status, err) = s.send(BOSS, job());
    assert_eq!(
        (status, code(&err)),
        (429, "rate_limited"),
        "two more do not fit: none is sent"
    );
    assert_eq!(
        s.send(BOSS, json!({"to": "w1", "kind": "notice", "body": "one"})).0,
        201,
        "one more still fits"
    );
    clock.advance(60);
    assert_eq!(s.send(BOSS, job()).0, 201, "a new minute");
    let (_, out) = s.get("/outbox", BOSS);
    assert_eq!(out["jobs"].as_array().map(Vec::len), Some(5));
}

#[test]
fn attachments_go_to_each_copy() {
    let s = Server::start(ROLES, racking_server::Clock::system());
    let (status, sent) = s.send(
        BOSS,
        json!({"to": ["w1", "w2"], "kind": "report", "title": "Logs", "body": "see file"}),
    );
    assert_eq!(status, 201);
    for (_, id) in made(&sent) {
        assert_eq!(s.attach(BOSS, &id, "probe.log", b"two devices\n"), 200, "{id}");
    }
    for token in [W1, W2] {
        let (_, inbox) = s.get("/inbox", token);
        let files = inbox["jobs"][0]["attachments"].clone();
        assert_eq!(files[0]["name"], "probe.log", "{inbox}");
        assert_eq!(files[0]["size"], 12);
    }
    let (status, notice) = s.send(BOSS, json!({"to": ["w1", "w2"], "kind": "notice", "body": "n"}));
    assert_eq!(status, 201);
    let id = made(&notice)[0].1.clone();
    assert_eq!(
        s.attach(BOSS, &id, "x.log", b"x"),
        409,
        "a notice takes no files, as for one"
    );
}

#[test]
fn whoami_names_the_recipient_limit() {
    let s = Server::start(ROLES, racking_server::Clock::system());
    let (_, me) = s.get("/whoami", W1);
    assert_eq!(me["limits"]["recipients"], 16);
}
