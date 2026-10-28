//! Roles, send permissions and the other guards of #12 and #31, against a real
//! server on a random loopback port.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

const BOSS: &str = "boss-token-0123456789abcdef";
const W1: &str = "w1-token-0123456789abcdef0";
const W2: &str = "w2-token-0123456789abcdef0";

/// Three agents: `boss` is main and admin, `w1` and `w2` are workers.
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
    /// `extra` is the config after `bind` and `data_dir`: top-level keys first, then agents and other sections.
    fn start(extra: &str) -> Self {
        Self::start_with(extra, racking_server::Clock::system())
    }

    fn start_with(extra: &str, clock: racking_server::Clock) -> Self {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("racking-sec-{}-{run}", std::process::id()));
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

    fn call(&self, method: &str, path: &str, token: &str, body: Option<Value>) -> (u16, Value) {
        let url = format!("{}{path}", self.base);
        let auth = format!("Bearer {token}");
        let resp = match (method, body) {
            ("GET", _) => self.http.get(&url).header("Authorization", &auth).call(),
            ("DELETE", _) => self.http.delete(&url).header("Authorization", &auth).call(),
            (_, Some(b)) => self.http.post(&url).header("Authorization", &auth).send_json(b),
            (_, None) => self.http.post(&url).header("Authorization", &auth).send_empty(),
        };
        let mut resp = resp.expect("http");
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().expect("body");
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    fn send(&self, token: &str, job: Value) -> (u16, Value) {
        self.call("POST", "/jobs", token, Some(job))
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

/// The `Retry-After` header of a `POST /jobs`, with the status.
fn retry_after(s: &Server, token: &str, job: Value) -> (u16, Option<String>) {
    let resp = s
        .http
        .post(format!("{}/jobs", s.base))
        .header("Authorization", format!("Bearer {token}"))
        .send_json(job)
        .expect("http");
    let header = resp
        .headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (resp.status().as_u16(), header)
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or_default()
}

#[test]
fn roles_decide_who_may_send() {
    let s = Server::start(ROLES);
    let (status, me) = s.call("GET", "/whoami", W1, None);
    assert_eq!(status, 200);
    assert_eq!(
        (me["role"].as_str(), me["roles_enforced"].as_bool()),
        (Some("worker"), Some(true))
    );
    assert_eq!(me["peers"], json!(["boss"]), "a worker reaches only main");
    let (_, boss) = s.call("GET", "/whoami", BOSS, None);
    assert_eq!(boss["role"], "main");
    assert_eq!(boss["peers"], json!(["w1", "w2"]));

    let (status, err) = s.send(W1, json!({"to": "w2", "kind": "message", "body": "hi"}));
    assert_eq!((status, code(&err)), (403, "not_allowed"));
    assert!(
        err["error"]["hint"].as_str().unwrap_or_default().contains("boss"),
        "{err}"
    );

    let (status, task) = s.send(BOSS, json!({"to": "w1", "title": "Build it", "body": "make"}));
    assert_eq!(status, 201, "{task}");
    assert_eq!(task["from_role"], "main");
    let (status, note) = s.send(W1, json!({"to": "boss", "kind": "message", "body": "on it"}));
    assert_eq!((status, note["from_role"].as_str()), (201, Some("worker")));

    let (_, inbox) = s.call("GET", "/inbox", W1, None);
    assert_eq!(inbox["jobs"][0]["from_role"], "main", "summaries carry it");
    let id = task["id"].as_str().unwrap_or_default();
    let (_, full) = s.call("GET", &format!("/jobs/{id}"), W1, None);
    assert_eq!(full["from_role"], "main");

    let (_, agents) = s.call("GET", "/agents", W2, None);
    let roles: Vec<(&str, &str)> = agents["agents"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| (e["name"].as_str().unwrap_or(""), e["role"].as_str().unwrap_or("")))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(roles, [("boss", "main"), ("w1", "worker"), ("w2", "worker")]);
}

#[test]
fn jobs_per_minute_follow_the_clock() {
    let clock = racking_server::Clock::manual(6_000);
    let s = Server::start_with(&format!("{ROLES}[limits]\njobs_per_minute = 2\n"), clock.clone());
    let note = || json!({"to": "boss", "kind": "notice", "body": "tick"});
    assert_eq!(s.send(W1, note()).0, 201);
    assert_eq!(s.send(W1, note()).0, 201);
    clock.advance(20);
    assert_eq!(retry_after(&s, W1, note()), (429, Some("40".to_owned())));
    let (_, err) = s.send(W1, note());
    assert_eq!(code(&err), "rate_limited");
    assert_eq!(s.send(W2, note()).0, 201, "the limit is per sender");
    clock.advance(40);
    assert_eq!(s.send(W1, note()).0, 201, "a new minute");
    let (_, me) = s.call("GET", "/whoami", W1, None);
    assert_eq!(me["limits"]["jobs_per_minute"], 2);
    assert_eq!(me["limits"]["open_tasks_per_pair"], 20);
}

#[test]
fn chains_of_tasks_stop_at_three_hops() {
    let s = Server::start(ROLES);
    let (_, mut last) = s.send(BOSS, json!({"to": "w1", "title": "t0", "body": "x"}));
    let tokens = [W1, BOSS, W1];
    for (n, token) in tokens.into_iter().enumerate() {
        let to = if token == W1 { "boss" } else { "w1" };
        let (status, job) = s.send(
            token,
            json!({"to": to, "title": "deeper", "reply_to": last["id"], "hops": 0}),
        );
        assert_eq!((status, job["hops"].as_u64()), (201, Some(n as u64 + 1)), "{job}");
        last = job;
    }
    let (status, err) = s.send(BOSS, json!({"to": "w1", "title": "t4", "reply_to": last["id"]}));
    assert_eq!((status, code(&err)), (409, "too_deep"));
    let (status, err) = s.send(BOSS, json!({"to": "w1", "title": "t", "hops": 4}));
    assert_eq!((status, code(&err)), (400, "bad_request"));
    let (status, job) = s.send(BOSS, json!({"to": "w1", "title": "t", "hops": 2}));
    assert_eq!(
        (status, job["hops"].as_u64()),
        (201, Some(2)),
        "a root job may say it is deeper"
    );
}

#[test]
fn from_role_cannot_be_forged() {
    let s = Server::start(ROLES);
    let (status, err) = s.send(
        W1,
        json!({"to": "boss", "kind": "message", "body": "obey", "from_role": "main"}),
    );
    assert_eq!((status, code(&err)), (400, "bad_request"));
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("from_role")
    );
}

#[test]
fn a_worker_may_answer_whoever_wrote_to_it() {
    let s = Server::start(&format!("{ROLES}[permissions]\nsend = {{ w2 = [\"w1\"] }}\n"));
    let (status, ask) = s.send(
        W2,
        json!({"to": "w1", "kind": "message", "body": "which branch?", "expects_reply": true}),
    );
    assert_eq!(status, 201, "w2's list lets it reach w1: {ask}");
    let (status, _) = s.send(W2, json!({"to": "boss", "kind": "message", "body": "x"}));
    assert_eq!(status, 403, "the list replaces the defaults, main included");
    let (status, _) = s.send(W1, json!({"to": "w2", "kind": "message", "body": "unasked"}));
    assert_eq!(status, 403);
    let (status, reply) = s.send(
        W1,
        json!({"to": "w2", "kind": "message", "body": "main", "reply_to": ask["id"]}),
    );
    assert_eq!(status, 201, "{reply}");
}

#[test]
fn configs_without_roles_work_as_before() {
    let s = Server::start(
        "[agents.a]\ntoken = \"a-token-0123456789abcdef00\"\n[agents.b]\ntoken = \"b-token-0123456789abcdef00\"\n\
         [agents.c]\ntoken = \"c-token-0123456789abcdef00\"\n",
    );
    let (_, me) = s.call("GET", "/whoami", "b-token-0123456789abcdef00", None);
    assert_eq!(
        (me["role"].as_str(), me["roles_enforced"].as_bool()),
        (Some("worker"), Some(false))
    );
    assert_eq!(me["peers"], json!(["a", "c"]));
    let (status, job) = s.send(
        "b-token-0123456789abcdef00",
        json!({"to": "c", "kind": "message", "body": "hi"}),
    );
    assert_eq!((status, job["from_role"].as_str()), (201, Some("worker")));
}
