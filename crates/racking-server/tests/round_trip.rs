//! The round trip of docs/protocol.md against a real server on a random loopback port.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const LINUX: &str = "linux-token-0123456789abcdef";
const WINDOWS: &str = "windows-token-0123456789abcdef";

struct Server {
    running: Option<racking_server::Running>,
    base: String,
    dir: PathBuf,
    http: ureq::Agent,
}

impl Server {
    fn start() -> Self {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("racking-it-{}-{run}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = racking_server::Config::parse(&format!(
            "bind = \"127.0.0.1:0\"\ndata_dir = {:?}\n[agents.linux]\ntoken = \"{LINUX}\"\nadmin = true\n\
             [agents.windows]\ntoken = \"{WINDOWS}\"\n",
            dir.display().to_string()
        ))
        .expect("config");
        let running = racking_server::start(&config).expect("start");
        let base = format!("http://{}", running.addr());
        let http = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(70)))
            .build()
            .new_agent();
        Self {
            running: Some(running),
            base,
            dir,
            http,
        }
    }

    fn call(&self, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> (u16, Value) {
        let url = format!("{}{path}", self.base);
        let auth = token.map(|t| format!("Bearer {t}"));
        let resp = match (method, body) {
            ("GET", _) => {
                let mut req = self.http.get(&url);
                if let Some(a) = &auth {
                    req = req.header("Authorization", a);
                }
                req.call()
            }
            (m, body) => {
                let mut req = if m == "PUT" {
                    self.http.put(&url)
                } else {
                    self.http.post(&url)
                };
                if let Some(a) = &auth {
                    req = req.header("Authorization", a);
                }
                match body {
                    Some(b) => req.send_json(b),
                    None => req.send_empty(),
                }
            }
        };
        let mut resp = resp.expect("http");
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().expect("body");
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    fn put_file(&self, path: &str, token: &str, bytes: &[u8], sha: &str, key: Option<&str>) -> (u16, Value) {
        let mut req = self
            .http
            .put(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {token}"))
            .header("X-Sha256", sha);
        if let Some(k) = key {
            req = req.header("Idempotency-Key", k);
        }
        let mut resp = req.send(bytes).expect("upload");
        let status = resp.status().as_u16();
        (status, resp.body_mut().read_json().unwrap_or(Value::Null))
    }
}

impl Server {
    fn publish(&self, path: &str, token: &str, bytes: &[u8], sha: &str, version: &str) -> (u16, Value) {
        let mut resp = self
            .http
            .put(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {token}"))
            .header("X-Sha256", sha)
            .header("X-Version", version)
            .send(bytes)
            .expect("publish");
        let status = resp.status().as_u16();
        (status, resp.body_mut().read_json().unwrap_or(Value::Null))
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

#[test]
fn protocol_round_trip() {
    let s = Server::start();

    let (status, guide) = s.call("GET", "/help", None, None);
    assert_eq!(status, 200);
    assert!(guide.as_str().is_some_and(|g| g.contains("POST /jobs")));
    let (status, err) = s.call("GET", "/whoami", None, None);
    assert_eq!((status, err["error"]["code"].as_str()), (401, Some("no_token")));
    let (status, err) = s.call("GET", "/whoami", Some("nope-nope-nope-nope-nope-nope"), None);
    assert_eq!((status, err["error"]["code"].as_str()), (401, Some("bad_token")));
    let (status, me) = s.call("GET", "/whoami", Some(LINUX), None);
    assert_eq!(
        (status, me["agent"].as_str(), me["peers"].clone()),
        (200, Some("linux"), json!(["windows"]))
    );

    // Linux starts waiting on its outbox before anything exists, to prove the wait wakes.
    let outbox_waiter = {
        let base = s.base.clone();
        let http = s.http.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let mut since = 0;
            loop {
                let mut resp = http
                    .get(format!("{base}/outbox/wait?since={since}&timeout=20"))
                    .header("Authorization", format!("Bearer {LINUX}"))
                    .call()
                    .expect("outbox wait");
                let v: Value = resp.body_mut().read_json().expect("json");
                if let Some(done) = v["jobs"]
                    .as_array()
                    .and_then(|jobs| jobs.iter().find(|j| j["state"] == "done"))
                {
                    return (done["id"].as_str().map(str::to_owned), started.elapsed());
                }
                since = v["next"].as_i64().expect("next");
                assert!(started.elapsed() < Duration::from_secs(30), "outbox never showed done");
            }
        })
    };

    let job = json!({"to":"windows","kind":"task","title":"Run psport-probe on Windows",
        "body":"Run `psport-probe.bat` and send the log.","tags":["decanter","windows"],
        "origin":{"repo":"decanter","id":"873"}});
    let send = |key: &str, body: &Value| {
        let mut resp = s
            .http
            .post(format!("{}/jobs", s.base))
            .header("Authorization", format!("Bearer {LINUX}"))
            .header("Idempotency-Key", key)
            .send_json(body)
            .expect("post");
        (
            resp.status().as_u16(),
            resp.body_mut().read_json::<Value>().expect("json"),
        )
    };
    let (status, j1) = send("7f3c", &job);
    assert_eq!(status, 201, "{j1}");
    assert_eq!(
        (j1["id"].as_str(), j1["from"].as_str(), j1["state"].as_str()),
        (Some("j1"), Some("linux"), Some("sent"))
    );
    assert_eq!(j1["seq"], 1);
    let (status, again) = send("7f3c", &job);
    assert_eq!(
        (status, again["id"].as_str()),
        (201, Some("j1")),
        "retry returns the stored answer"
    );
    let mut changed = job.clone();
    changed["title"] = json!("Something else");
    let (status, err) = send("7f3c", &changed);
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (422, Some("idempotency_conflict"))
    );

    let bat = b"@echo off\r\npsport-probe.exe > probe.log\r\n";
    let (status, err) = s.put_file(
        "/jobs/j1/attachments/psport-probe.bat",
        LINUX,
        bat,
        &"0".repeat(64),
        None,
    );
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (422, Some("checksum_mismatch"))
    );
    let (status, att) = s.put_file(
        "/jobs/j1/attachments/psport-probe.bat",
        LINUX,
        bat,
        &sha(bat),
        Some("up1"),
    );
    assert_eq!((status, att["size"].as_u64()), (200, Some(bat.len() as u64)));
    let (status, _) = s.put_file("/jobs/j1/attachments/../x", LINUX, bat, &sha(bat), None);
    assert_eq!(status, 404, "path parts are not a route");

    // Windows wakes, reads and answers.
    let (status, inbox) = s.call("GET", "/inbox/wait?since=0&timeout=25", Some(WINDOWS), None);
    assert_eq!((status, inbox["next"].as_i64()), (200, Some(1)));
    let summary = &inbox["jobs"][0];
    assert_eq!(
        (summary["id"].as_str(), summary.get("body").is_none()),
        (Some("j1"), true)
    );
    assert_eq!(
        summary["body_preview"].as_str(),
        Some("Run `psport-probe.bat` and send the log.")
    );

    let (status, err) = s.call("POST", "/jobs/j1/accept", Some(WINDOWS), None);
    assert_eq!((status, err["error"]["code"].as_str()), (409, Some("bad_transition")));
    let (status, full) = s.call("GET", "/jobs/j1", Some(WINDOWS), None);
    assert_eq!((status, full["state"].as_str()), (200, Some("received")));
    assert_eq!(full["attachments"][0]["sha256"].as_str(), Some(sha(bat).as_str()));

    let mut dl = s
        .http
        .get(format!("{}/jobs/j1/attachments/psport-probe.bat", s.base))
        .header("Authorization", format!("Bearer {WINDOWS}"))
        .call()
        .expect("download");
    let header_sha = dl
        .headers()
        .get("x-sha256")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = dl.body_mut().read_to_vec().expect("bytes");
    assert_eq!((bytes.as_slice(), header_sha), (&bat[..], Some(sha(bat))));

    let (status, err) = s.put_file("/jobs/j1/attachments/late.txt", LINUX, b"x", &sha(b"x"), None);
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (409, Some("attachments_locked"))
    );
    let (status, err) = s.call("POST", "/jobs/j1/cancel", Some(LINUX), None);
    assert_eq!((status, err["error"]["code"].as_str()), (409, Some("bad_transition")));

    let (status, acc) = s.call("POST", "/jobs/j1/accept", Some(WINDOWS), None);
    assert_eq!((status, acc["state"].as_str()), (200, Some("accepted")));
    let (status, acc) = s.call("POST", "/jobs/j1/accept", Some(WINDOWS), None);
    assert_eq!(
        (status, acc["state"].as_str()),
        (200, Some("accepted")),
        "repeating a move is a no-op"
    );
    let (status, link) = s.call(
        "PUT",
        "/links/j1",
        Some(WINDOWS),
        Some(json!({"repo":"decanter","id":"31"})),
    );
    assert_eq!((status, link["id"].as_str()), (200, Some("31")));
    let (status, err) = s.call(
        "PUT",
        "/links/j1",
        Some(LINUX),
        Some(json!({"repo":"decanter","id":"1"})),
    );
    assert_eq!((status, err["error"]["code"].as_str()), (403, Some("not_yours")));

    let result = json!({"to":"linux","kind":"result","reply_to":"j1","title":"Probe finished",
        "body":"Two Vulkan devices. Log attached.","tags":["decanter"]});
    let (status, j2) = s.call("POST", "/jobs", Some(WINDOWS), Some(result));
    assert_eq!(status, 201, "{j2}");
    assert_eq!(
        (j2["id"].as_str(), j2["seq"].as_i64(), j2["outcome"].as_str()),
        (Some("j2"), Some(1), Some("done"))
    );
    let log = b"device 0: radv\ndevice 1: llvmpipe\n";
    let (status, _) = s.put_file("/jobs/j2/attachments/probe.log", WINDOWS, log, &sha(log), None);
    assert_eq!(status, 200);

    let (done_id, waited) = outbox_waiter.join().expect("outbox thread");
    assert_eq!(done_id.as_deref(), Some("j1"));
    assert!(
        waited < Duration::from_secs(20),
        "woken by the change, not by the timeout"
    );

    let (status, inbox) = s.call("GET", "/inbox/wait?since=0&timeout=1", Some(LINUX), None);
    assert_eq!((status, inbox["jobs"][0]["id"].as_str()), (200, Some("j2")));
    let (status, links) = s.call("GET", "/links?job=j1", Some(LINUX), None);
    assert_eq!((status, links["links"][0]["id"].as_str()), (200, Some("31")));

    // A timed-out wait returns an empty list with the same cursor.
    let started = Instant::now();
    let (status, empty) = s.call("GET", "/inbox/wait?since=1&timeout=1", Some(LINUX), None);
    assert_eq!(
        (status, empty["jobs"].as_array().map(Vec::len), empty["next"].as_i64()),
        (200, Some(0), Some(1))
    );
    assert!(started.elapsed() >= Duration::from_millis(900));

    let (status, err) = s.call("GET", "/inbox?bogus=1", Some(LINUX), None);
    assert_eq!((status, err["error"]["code"].as_str()), (400, Some("bad_request")));
    let (status, err) = s.call("POST", "/jobs", Some(LINUX), Some(json!({"to":"mac","title":"x"})));
    assert_eq!((status, err["error"]["code"].as_str()), (404, Some("not_found")));
    assert!(err["error"]["hint"].as_str().is_some_and(|h| h.contains("windows")));
}

#[test]
fn dist_publish_and_download() {
    let s = Server::start();
    let linux_rack = b"#!/bin/sh\necho rack 0.2.0\n";
    let path = "/dist/x86_64-unknown-linux-gnu/rack";

    let (status, m) = s.call("GET", "/dist/manifest", Some(WINDOWS), None);
    assert_eq!((status, m), (200, json!({"version": null, "targets": {}})));
    let (status, err) = s.call("GET", "/dist/manifest", None, None);
    assert_eq!((status, err["error"]["code"].as_str()), (401, Some("no_token")));
    let (status, err) = s.call("GET", path, Some(WINDOWS), None);
    assert_eq!((status, err["error"]["code"].as_str()), (404, Some("not_found")));

    let (status, err) = s.publish(path, WINDOWS, linux_rack, &sha(linux_rack), "0.2.0");
    assert_eq!((status, err["error"]["code"].as_str()), (403, Some("not_admin")));
    assert!(
        err["error"]["hint"]
            .as_str()
            .is_some_and(|h| h.contains("admin = true"))
    );
    let (status, err) = s.publish(path, LINUX, linux_rack, &sha(b"other"), "0.2.0");
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (422, Some("checksum_mismatch"))
    );
    let (status, err) = s.publish(path, LINUX, linux_rack, &sha(linux_rack), "v0.2");
    assert_eq!((status, err["error"]["code"].as_str()), (400, Some("bad_request")));
    let (status, err) = s.publish(
        "/dist/x86_64-apple-darwin/rack",
        LINUX,
        linux_rack,
        &sha(linux_rack),
        "0.2.0",
    );
    assert_eq!((status, err["error"]["code"].as_str()), (400, Some("bad_request")));
    let (status, err) = s.publish(
        "/dist/x86_64-unknown-linux-gnu/rack.exe",
        LINUX,
        linux_rack,
        &sha(linux_rack),
        "0.2.0",
    );
    assert_eq!((status, err["error"]["code"].as_str()), (404, Some("not_found")));

    let (status, published) = s.publish(path, LINUX, linux_rack, &sha(linux_rack), "0.2.0");
    assert_eq!((status, published["version"].as_str()), (200, Some("0.2.0")));
    let exe = b"MZ fake windows rack";
    let (status, _) = s.publish("/dist/x86_64-pc-windows-gnu/rack.exe", LINUX, exe, &sha(exe), "0.2.1");
    assert_eq!(status, 200);

    let (status, m) = s.call("GET", "/dist/manifest", Some(WINDOWS), None);
    assert_eq!(status, 200);
    assert_eq!(m["version"], "0.2.1", "the top version is the last published");
    assert_eq!(
        m["targets"]["x86_64-unknown-linux-gnu"],
        json!({"version": "0.2.0", "file": "rack", "size": linux_rack.len(), "sha256": sha(linux_rack)})
    );
    assert_eq!(m["targets"]["x86_64-pc-windows-gnu"]["file"], "rack.exe");

    let mut resp = s
        .http
        .get(format!("{}/dist/x86_64-pc-windows-gnu/rack.exe", s.base))
        .header("Authorization", format!("Bearer {WINDOWS}"))
        .call()
        .expect("download");
    assert_eq!(resp.status().as_u16(), 200);
    let header = resp
        .headers()
        .get("x-sha256")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = resp.body_mut().read_to_vec().expect("bytes");
    assert_eq!((body.as_slice(), header), (&exe[..], Some(sha(exe))));

    let (status, guide) = s.call("GET", "/help", None, None);
    assert_eq!(status, 200);
    assert!(
        guide
            .as_str()
            .is_some_and(|g| g.contains("PUT /dist/{target}/{file}") && g.contains("not_admin"))
    );
}
