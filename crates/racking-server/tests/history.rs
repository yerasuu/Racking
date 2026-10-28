//! `GET /threads/{id}` and `GET /jobs` against a real server on a random loopback port.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

const LINUX: &str = "linux-token-0123456789abcdef";
const WINDOWS: &str = "windows-token-0123456789abcdef";
const MAC: &str = "mac-token-0123456789abcdef0";

struct Server {
    running: Option<racking_server::Running>,
    base: String,
    dir: PathBuf,
    http: ureq::Agent,
}

impl Server {
    /// Three agents of the config file and no roles, so each may write to each.
    fn start() -> Self {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("racking-hist-{}-{run}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = racking_server::Config::parse(&format!(
            "bind = \"127.0.0.1:0\"\ndata_dir = {:?}\n[agents.linux]\ntoken = \"{LINUX}\"\n\
             [agents.windows]\ntoken = \"{WINDOWS}\"\n[agents.mac]\ntoken = \"{MAC}\"\n",
            dir.display().to_string()
        ))
        .expect("config");
        let running = racking_server::start(&config).expect("start");
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

    fn get(&self, path: &str, token: &str) -> (u16, Value) {
        let mut resp = self
            .http
            .get(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {token}"))
            .call()
            .expect("http");
        let status = resp.status().as_u16();
        (status, resp.body_mut().read_json().unwrap_or(Value::Null))
    }

    fn post(&self, path: &str, token: &str, body: Option<Value>) -> Value {
        let req = self
            .http
            .post(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {token}"));
        let mut resp = match body {
            Some(b) => req.send_json(b),
            None => req.send_empty(),
        }
        .expect("http");
        let status = resp.status().as_u16();
        let v: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
        assert!((200..300).contains(&status), "POST {path}: {status} {v}");
        v
    }

    /// Sends a job and returns its id.
    fn send(&self, token: &str, job: Value) -> String {
        self.post("/jobs", token, Some(job))["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
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

fn ids(v: &Value) -> Vec<&str> {
    v["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|j| j["id"].as_str())
        .collect()
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or_default()
}

#[test]
fn two_agents_read_the_same_conversation() {
    let s = Server::start();
    let task = s.send(
        LINUX,
        json!({"to": "windows", "title": "Run the probe", "body": "Run it and send the log."}),
    );
    s.post(&format!("/jobs/{task}/receive"), WINDOWS, None);
    s.post(&format!("/jobs/{task}/accept"), WINDOWS, None);
    let comment = s.send(
        LINUX,
        json!({"to": "windows", "kind": "comment", "reply_to": task, "title": "Also", "body": "Use the debug build."}),
    );
    let question = s.send(
        WINDOWS,
        json!({"to": "linux", "kind": "message", "reply_to": comment, "body": "Which debug build?", "expects_reply": true}),
    );
    let answer = s.send(
        LINUX,
        json!({"to": "windows", "kind": "message", "reply_to": question, "body": "The one from today."}),
    );
    let result = s.send(
        WINDOWS,
        json!({"to": "linux", "kind": "result", "reply_to": task, "title": "Probe done", "body": "Two devices."}),
    );
    let other = s.send(MAC, json!({"to": "linux", "kind": "message", "body": "unrelated"}));

    let (status, from_linux) = s.get(&format!("/threads/{answer}"), LINUX);
    assert_eq!(status, 200, "{from_linux}");
    let (_, from_windows) = s.get(&format!("/threads/{task}"), WINDOWS);
    assert_eq!(from_linux, from_windows, "the same conversation from either side");
    assert_eq!(ids(&from_linux), [&task, &comment, &question, &answer, &result]);
    assert_eq!(from_linux["root"], task.as_str());
    assert_eq!(
        (&from_linux["omitted"], &from_linux["truncated"], &from_linux["next"]),
        (&json!(0), &json!(false), &Value::Null)
    );
    let question_entry = &from_linux["jobs"][2];
    assert_eq!(
        (
            &question_entry["depth"],
            &question_entry["reply_to"],
            &question_entry["from"]
        ),
        (&json!(2), &json!(comment), &json!("windows"))
    );
    assert_eq!(question_entry["body"], "Which debug build?");
    assert!(from_linux["jobs"][0].get("body").is_none(), "a task comes as a summary");
    assert_eq!(from_linux["jobs"][0]["state"], "done");
    assert!(!from_linux.to_string().contains(&format!("\"{other}\"")));

    let (_, full) = s.get(&format!("/threads/{task}?full=1&limit=2"), LINUX);
    assert_eq!(full["jobs"][0]["body"], "Run it and send the log.");
    assert_eq!((ids(&full).len(), &full["truncated"]), (2, &json!(true)));
    let next = full["next"].as_str().expect("next");
    let (_, rest) = s.get(&format!("/threads/{task}?full=1&after={next}"), LINUX);
    assert_eq!(ids(&rest), [&question, &answer, &result]);

    let (status, err) = s.get(&format!("/threads/{task}"), MAC);
    assert_eq!((status, code(&err)), (403, "not_yours"));
    assert!(!err.to_string().contains("Run the probe"), "no title leaks: {err}");
    let (status, err) = s.get("/threads/j999", LINUX);
    assert_eq!((status, code(&err)), (404, "not_found"));
}

#[test]
fn reading_a_thread_or_searching_delivers_nothing() {
    let s = Server::start();
    let (_, outbox) = s.get("/outbox?since=0", LINUX);
    let notice = s.send(
        LINUX,
        json!({"to": "windows", "kind": "notice", "body": "Deploying the GPU driver now"}),
    );
    let (_, before) = s.get("/outbox?since=0", LINUX);
    let (status, thread) = s.get(&format!("/threads/{notice}?full=1"), WINDOWS);
    assert_eq!(status, 200, "{thread}");
    assert_eq!(thread["jobs"][0]["state"], "sent");
    let (_, found) = s.get("/jobs?q=gpu%20driver", WINDOWS);
    assert_eq!(ids(&found), [&notice]);
    let (_, after) = s.get(&format!("/outbox?since={}", before["next"]), LINUX);
    assert!(ids(&after).is_empty(), "the sender saw no change: {after}");
    assert_ne!(outbox["next"], before["next"]);
    let (_, inbox) = s.get("/inbox?since=0", WINDOWS);
    assert_eq!(
        (ids(&inbox), &inbox["jobs"][0]["state"]),
        (vec![notice.as_str()], &json!("received")),
        "the notice still arrives as new"
    );
}

#[test]
fn search_by_text_kind_and_cursor() {
    let s = Server::start();
    let report = s.send(
        WINDOWS,
        json!({"to": "linux", "kind": "report", "title": "Vulkan probe", "body": format!("{}needle at the end", "x".repeat(300))}),
    );
    let message = s.send(
        LINUX,
        json!({"to": "windows", "kind": "message", "body": "50% done, ñandú"}),
    );
    let task = s.send(
        LINUX,
        json!({"to": "windows", "title": "Look at the vulkan crash", "body": "b"}),
    );
    s.send(
        MAC,
        json!({"to": "windows", "kind": "message", "body": "vulkan from mac"}),
    );

    let (status, by_text) = s.get("/jobs?q=VULKAN", LINUX);
    assert_eq!(status, 200, "{by_text}");
    assert_eq!(ids(&by_text), [&task, &report], "newest first, only the caller's jobs");
    let (_, in_body) = s.get("/jobs?q=needle+at", LINUX);
    assert_eq!(ids(&in_body), [&report], "the body is searched too");
    assert!(in_body["jobs"][0].get("body").is_none(), "previews only");
    assert_eq!(in_body["jobs"][0]["body_truncated"], true);
    let (_, kind) = s.get("/jobs?q=vulkan&kind=report&from=windows", LINUX);
    assert_eq!(ids(&kind), [&report]);
    let (_, percent) = s.get("/jobs?q=50%25", WINDOWS);
    assert_eq!(ids(&percent), [&message]);
    let (_, accent) = s.get("/jobs?q=%C3%B1and%C3%BA", LINUX);
    assert_eq!(ids(&accent), [&message]);

    let (_, first) = s.get("/jobs?limit=2", LINUX);
    assert_eq!(ids(&first), [&task, &message]);
    let next = first["next"].as_str().expect("next");
    let (_, second) = s.get(&format!("/jobs?limit=2&after={next}"), LINUX);
    assert_eq!(ids(&second), [&report]);
    assert_eq!(second["next"], Value::Null);
    let (_, today) = s.get("/jobs?since=2020-01-01&until=2999-01-01T00:00:00Z", LINUX);
    assert_eq!(ids(&today).len(), 3);

    for bad in [
        format!("/jobs?q={}", "x".repeat(101)),
        "/jobs?q=".to_owned(),
        "/jobs?q=%zz".to_owned(),
        "/jobs?kind=order".to_owned(),
        "/jobs?state=open".to_owned(),
        "/jobs?from=Linux".to_owned(),
        "/jobs?since=yesterday".to_owned(),
        "/jobs?since=2026-10-28&until=2026-10-27".to_owned(),
        "/jobs?limit=0".to_owned(),
        "/jobs?limit=201".to_owned(),
        "/jobs?after=nope".to_owned(),
        "/jobs?body=1".to_owned(),
        "/threads/j1?full=yes".to_owned(),
        "/threads/x1".to_owned(),
    ] {
        let (status, err) = s.get(&bad, LINUX);
        assert_eq!((status, code(&err)), (400, "bad_request"), "{bad}: {err}");
        assert!(!err["error"]["hint"].as_str().unwrap_or_default().is_empty(), "{bad}");
    }
}
