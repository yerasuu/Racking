//! Graceful shutdown (#56): the real `racking-server` binary on a random loopback
//! port, stopped with `kill -TERM` as Docker and systemd stop it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const W1: &str = "w1-token-0123456789abcdef0";

const ROLES: &str = "[agents.boss]\ntoken = \"boss-token-0123456789abcdef\"\nrole = \"main\"\n\
                     [agents.w1]\ntoken = \"w1-token-0123456789abcdef0\"\n";

struct Process {
    child: Child,
    addr: String,
    dir: PathBuf,
    log: mpsc::Receiver<String>,
}

impl Process {
    fn start(grace_secs: u64) -> Self {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("racking-stop-{}-{run}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let config = dir.join("server.toml");
        std::fs::write(
            &config,
            format!(
                "bind = \"127.0.0.1:0\"\nshutdown_grace_secs = {grace_secs}\ndata_dir = {:?}\n{ROLES}",
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
        let mut addr = String::new();
        let mut line = String::new();
        while lines.read_line(&mut line).expect("log") > 0 {
            if let Some(rest) = line.split("addr=").nth(1) {
                addr = rest.split_whitespace().next().unwrap_or_default().to_owned();
                break;
            }
            line.clear();
        }
        assert!(!addr.is_empty(), "the server did not say where it listens");
        // Read on in the background so the server never blocks on a full pipe.
        let (tx, log) = mpsc::channel();
        std::thread::spawn(move || {
            let mut rest = String::new();
            let _ = lines.read_to_string(&mut rest);
            let _ = tx.send(rest);
        });
        Self { child, addr, dir, log }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn signal(&self, name: &str) {
        let status = Command::new("kill")
            .args([&format!("-{name}"), "--", &self.child.id().to_string()])
            .status()
            .expect("kill");
        assert!(status.success());
    }

    fn exit_within(&mut self, limit: Duration) -> ExitStatus {
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            assert!(started.elapsed() < limit, "still running after {limit:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Everything logged after `listening`; the server must have exited.
    fn log(&self) -> String {
        self.log.recv_timeout(Duration::from_secs(5)).unwrap_or_default()
    }

    fn db(&self) -> PathBuf {
        self.dir.join("data").join("racking.db")
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .new_agent()
}

/// A job body larger than the 1 KiB tiny_http reads before handing the request
/// over, so the handler is still reading it when the signal comes.
fn big_job() -> Vec<u8> {
    json!({"to": "boss", "kind": "message", "body": "x".repeat(4000)})
        .to_string()
        .into_bytes()
}

fn wal_len(p: &Process) -> u64 {
    std::fs::metadata(p.dir.join("data").join("racking.db-wal")).map_or(0, |m| m.len())
}

fn jobs_in(p: &Process) -> i64 {
    let conn = rusqlite::Connection::open(p.db()).expect("open db");
    let check: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .expect("integrity");
    assert_eq!(check, "ok");
    conn.query_row("SELECT count(*) FROM jobs", [], |r| r.get(0))
        .expect("count")
}

#[test]
fn sigterm_answers_long_polls_and_leaves_a_checkpointed_database() {
    let mut p = Process::start(10);
    let agent = http();
    let sent = agent
        .post(p.url("/jobs"))
        .header("Authorization", &format!("Bearer {W1}"))
        .send_json(json!({"to": "boss", "kind": "message", "body": "before the stop"}))
        .expect("send");
    assert_eq!(sent.status().as_u16(), 201);
    let poll = {
        let url = p.url("/inbox/wait?timeout=55");
        std::thread::spawn(move || {
            let started = Instant::now();
            let mut resp = http()
                .get(&url)
                .header("Authorization", &format!("Bearer {W1}"))
                .call()
                .expect("wait");
            let body: Value = resp.body_mut().read_json().expect("json");
            (resp.status().as_u16(), body, started.elapsed())
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    p.signal("TERM");
    let (status, body, took) = poll.join().expect("poll");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["jobs"].as_array().map(Vec::len), Some(0));
    assert!(
        took < Duration::from_secs(5),
        "the long poll answered at once: {took:?}"
    );
    let exit = p.exit_within(Duration::from_secs(12));
    assert_eq!(exit.code(), Some(0));
    let log = p.log();
    assert!(log.contains("database checkpointed"), "{log}");
    assert_eq!(wal_len(&p), 0, "the WAL was moved into the database");
    assert_eq!(jobs_in(&p), 1);
}

#[test]
fn a_request_in_flight_finishes_before_the_exit() {
    let mut p = Process::start(10);
    let body = big_job();
    let mut conn = TcpStream::connect(&p.addr).expect("connect");
    conn.set_read_timeout(Some(Duration::from_secs(20))).expect("timeout");
    let head = format!(
        "POST /jobs HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {W1}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        p.addr,
        body.len()
    );
    conn.write_all(head.as_bytes()).expect("head");
    conn.write_all(&body[..100]).expect("start of body");
    std::thread::sleep(Duration::from_millis(300));
    p.signal("TERM");
    std::thread::sleep(Duration::from_millis(500));
    assert!(p.child.try_wait().expect("try_wait").is_none(), "waits for the upload");
    assert!(TcpStream::connect(&p.addr).is_err(), "the listener is closed");
    conn.write_all(&body[100..]).expect("rest of body");
    let mut answer = String::new();
    conn.read_to_string(&mut answer).expect("answer");
    assert!(answer.starts_with("HTTP/1.1 201"), "{answer}");
    assert_eq!(p.exit_within(Duration::from_secs(12)).code(), Some(0));
    let log = p.log();
    assert!(
        log.contains("waiting for requests in flight") && log.contains("no requests in flight"),
        "{log}"
    );
    assert_eq!(jobs_in(&p), 1, "the job sent during the shutdown is kept");
}

#[test]
fn a_second_signal_exits_at_once() {
    let mut p = Process::start(30);
    let body = big_job();
    let mut conn = TcpStream::connect(&p.addr).expect("connect");
    let head = format!(
        "POST /jobs HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {W1}\r\nContent-Length: {}\r\n\r\n",
        p.addr,
        body.len()
    );
    conn.write_all(head.as_bytes()).expect("head");
    conn.write_all(&body[..100]).expect("start of body");
    std::thread::sleep(Duration::from_millis(300));
    p.signal("TERM");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        p.child.try_wait().expect("try_wait").is_none(),
        "the stalled upload holds the first one"
    );
    p.signal("INT");
    assert_eq!(p.exit_within(Duration::from_secs(3)).code(), Some(128 + 2));
}

#[test]
fn the_grace_period_bounds_a_stalled_request() {
    let mut p = Process::start(1);
    let body = big_job();
    let mut conn = TcpStream::connect(&p.addr).expect("connect");
    let head = format!(
        "POST /jobs HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {W1}\r\nContent-Length: {}\r\n\r\n",
        p.addr,
        body.len()
    );
    conn.write_all(head.as_bytes()).expect("head");
    conn.write_all(&body[..100]).expect("start of body");
    std::thread::sleep(Duration::from_millis(300));
    p.signal("TERM");
    assert_eq!(p.exit_within(Duration::from_secs(5)).code(), Some(0));
    let log = p.log();
    assert!(log.contains("grace period over"), "{log}");
    assert_eq!(jobs_in(&p), 0, "a cut-off upload is not kept");
}
