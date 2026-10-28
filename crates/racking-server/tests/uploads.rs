//! Resumable uploads, quota and the free-space reserve against a real server on a loopback port.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const LINUX: &str = "linux-token-0123456789abcdef";
const WINDOWS: &str = "windows-token-0123456789abcdef";
const CHUNK: usize = 256 * 1024;

struct Server {
    running: Option<racking_server::Running>,
    base: String,
    addr: String,
    dir: PathBuf,
    http: ureq::Agent,
}

impl Server {
    /// A server with small limits: 256 KiB chunks, 8 MiB files, 12 MiB per job and per agent.
    fn start() -> Self {
        Self::start_with(
            "[limits]\nmax_file_bytes = 8388608\nmax_job_bytes = 12582912\nupload_chunk_bytes = 262144\n\
             [quota]\nper_agent_bytes = 12582912\ndisk_free_min_bytes = 0\n",
        )
    }

    fn start_with(extra: &str) -> Self {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("racking-up-{}-{run}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = racking_server::Config::parse(&format!(
            "bind = \"127.0.0.1:0\"\ndata_dir = {:?}\n[agents.linux]\ntoken = \"{LINUX}\"\n\
             [agents.windows]\ntoken = \"{WINDOWS}\"\n{extra}",
            dir.display().to_string()
        ))
        .expect("config");
        let running = racking_server::start(&config).expect("start");
        let addr = running.addr().to_string();
        let http = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(120)))
            .build()
            .new_agent();
        Self {
            running: Some(running),
            base: format!("http://{addr}"),
            addr,
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
        (status, resp.body_mut().read_json().unwrap_or(Value::Null))
    }

    fn chunk(&self, upload: &str, token: &str, bytes: &[u8], first: usize, total: usize) -> (u16, Value) {
        let last = first + bytes.len() - 1;
        let mut resp = self
            .http
            .put(format!("{}/uploads/{upload}", self.base))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Range", format!("bytes {first}-{last}/{total}"))
            .send(bytes)
            .expect("chunk");
        let status = resp.status().as_u16();
        (status, resp.body_mut().read_json().unwrap_or(Value::Null))
    }

    /// Sends `data[from..]` chunk by chunk, asserting each answer.
    fn send_from(&self, upload: &str, data: &[u8], from: usize) {
        let mut at = from;
        while at < data.len() {
            let end = (at + CHUNK).min(data.len());
            let (status, body) = self.chunk(upload, LINUX, &data[at..end], at, data.len());
            assert_eq!(status, 200, "{body}");
            assert_eq!(body["received"].as_u64(), Some(end as u64));
            at = end;
        }
    }

    fn task(&self) -> String {
        let (status, job) = self.call(
            "POST",
            "/jobs",
            LINUX,
            Some(json!({"to": "windows", "title": "Look at the image", "body": "attached"})),
        );
        assert_eq!(status, 201, "{job}");
        job["id"].as_str().expect("id").to_owned()
    }

    fn start_upload(&self, job: &str, name: &str, data: &[u8]) -> (u16, Value) {
        self.call(
            "POST",
            &format!("/jobs/{job}/uploads"),
            LINUX,
            Some(json!({"name": name, "size": data.len(), "sha256": sha(data)})),
        )
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

/// Bytes that differ from offset to offset, so a chunk written at the wrong place shows in the hash.
fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
}

/// Opens a chunk upload by hand, sends `sent` of its bytes and drops the connection, as a
/// client whose network fails mid-chunk.
fn cut_chunk(s: &Server, upload: &str, chunk: &[u8], first: usize, total: usize, sent: usize) {
    let mut tcp = TcpStream::connect(&s.addr).expect("connect");
    let head = format!(
        "PUT /uploads/{upload} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {LINUX}\r\nContent-Length: {}\r\n\
         Content-Range: bytes {first}-{}/{total}\r\n\r\n",
        s.addr,
        chunk.len(),
        first + chunk.len() - 1
    );
    tcp.write_all(head.as_bytes()).expect("head");
    tcp.write_all(&chunk[..sent]).expect("part");
    tcp.flush().expect("flush");
    drop(tcp);
}

fn received(s: &Server, upload: &str) -> u64 {
    let (status, body) = s.call("GET", &format!("/uploads/{upload}"), LINUX, None);
    assert_eq!(status, 200, "{body}");
    body["received"].as_u64().expect("received")
}

/// Uploads `len` bytes, cutting the connection in the middle of the second chunk,
/// then resumes from the offset the server reports.
fn upload_with_a_cut(s: &Server, len: usize) {
    let job = s.task();
    let file = data(len);
    let (status, session) = s.start_upload(&job, "disk.img", &file);
    assert_eq!(status, 201, "{session}");
    assert_eq!(session["received"].as_u64(), Some(0));
    assert_eq!(session["state"].as_str(), Some("open"));
    assert!(session["expires_at"].as_str().is_some_and(|t| t.ends_with('Z')));
    let chunk_size = session["chunk_size"].as_u64().expect("chunk_size") as usize;
    assert_eq!(chunk_size, CHUNK);
    let upload = session["upload_id"].as_str().expect("id").to_owned();

    let (status, body) = s.chunk(&upload, LINUX, &file[..CHUNK], 0, len);
    assert_eq!((status, body["received"].as_u64()), (200, Some(CHUNK as u64)), "{body}");
    cut_chunk(s, &upload, &file[CHUNK..2 * CHUNK], CHUNK, len, 100_000);
    // The server notices the cut once the socket closes; the bytes that arrived are kept.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut at = received(s, &upload);
    while at != (CHUNK + 100_000) as u64 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        at = received(s, &upload);
    }
    assert_eq!(at, (CHUNK + 100_000) as u64, "the cut chunk keeps what arrived");

    // A client that lost its session id finds it by name, size and sha256 and continues.
    let (status, again) = s.start_upload(&job, "disk.img", &file);
    assert_eq!((status, again["upload_id"].as_str()), (200, Some(upload.as_str())));
    let (status, listed) = s.call("GET", &format!("/jobs/{job}/uploads"), LINUX, None);
    assert_eq!(status, 200);
    assert_eq!(listed["uploads"][0]["received"].as_u64(), Some(at));
    s.send_from(&upload, &file, at as usize);

    let (status, done) = s.call("POST", &format!("/uploads/{upload}/complete"), LINUX, None);
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["state"].as_str(), Some("complete"));
    assert_eq!(done["attachment"]["size"].as_u64(), Some(len as u64));
    assert_eq!(done["attachment"]["sha256"].as_str(), Some(sha(&file).as_str()));

    let mut resp = s
        .http
        .get(format!("{}/jobs/{job}/attachments/disk.img", s.base))
        .header("Authorization", format!("Bearer {WINDOWS}"))
        .call()
        .expect("download");
    assert_eq!(resp.status().as_u16(), 200);
    let mut hasher = Sha256::new();
    let mut reader = resp.body_mut().with_config().limit(u64::MAX).reader();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = reader.read(&mut buf).expect("read");
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(got, sha(&file));
}

#[test]
fn chunked_upload_survives_a_cut_connection() {
    let s = Server::start();
    upload_with_a_cut(&s, 3 * 1024 * 1024 + 1234);
}

/// The same run with a file larger than the one-request limit; slow in a debug build.
#[test]
#[ignore = "sends 300 MiB through the loopback"]
fn chunked_upload_of_hundreds_of_mib() {
    let s = Server::start_with("[limits]\nupload_chunk_bytes = 262144\n[quota]\ndisk_free_min_bytes = 0\n");
    upload_with_a_cut(&s, 300 * 1024 * 1024 + 17);
}

#[test]
fn offsets_checksums_and_aborts() {
    let s = Server::start();
    let job = s.task();
    let file = data(600 * 1024);
    let (_, session) = s.start_upload(&job, "a.bin", &file);
    let upload = session["upload_id"].as_str().expect("id").to_owned();

    let (status, err) = s.chunk(&upload, LINUX, &file[CHUNK..2 * CHUNK], CHUNK, file.len());
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (409, Some("bad_offset")),
        "{err}"
    );
    assert_eq!(err["error"]["received"].as_u64(), Some(0));
    let (status, _) = s.chunk(&upload, LINUX, &file[..CHUNK], 0, file.len());
    assert_eq!(status, 200);
    let (status, same) = s.chunk(&upload, LINUX, &file[..CHUNK], 0, file.len());
    assert_eq!(
        (status, same["received"].as_u64()),
        (200, Some(CHUNK as u64)),
        "a repeated chunk is a no-op"
    );
    let (status, err) = s.call("POST", &format!("/uploads/{upload}/complete"), LINUX, None);
    assert_eq!((status, err["error"]["code"].as_str()), (409, Some("bad_offset")));

    let (status, err) = s.chunk(&upload, LINUX, &file[..CHUNK + 10], 0, file.len());
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (413, Some("too_large")),
        "over chunk_size"
    );
    let bad = s
        .http
        .put(format!("{}/uploads/{upload}", s.base))
        .header("Authorization", format!("Bearer {LINUX}"))
        .header("Content-Range", "bytes 0-9/*")
        .send(&file[..10])
        .expect("put");
    assert_eq!(bad.status().as_u16(), 400);

    let (status, err) = s.call("GET", &format!("/uploads/{upload}"), WINDOWS, None);
    assert_eq!((status, err["error"]["code"].as_str()), (403, Some("not_yours")));
    let (status, _) = s.chunk(&upload, WINDOWS, &file[CHUNK..2 * CHUNK], CHUNK, file.len());
    assert_eq!(status, 403);
    let (status, _) = s.call("POST", &format!("/uploads/{upload}/complete"), WINDOWS, None);
    assert_eq!(status, 403);
    let (status, _) = s.call("DELETE", &format!("/uploads/{upload}"), WINDOWS, None);
    assert_eq!(status, 403);
    let (status, listed) = s.call("GET", &format!("/jobs/{job}/uploads"), WINDOWS, None);
    assert_eq!((status, listed["uploads"].as_array().map(Vec::len)), (200, Some(0)));

    let (status, _) = s.call("DELETE", &format!("/uploads/{upload}"), LINUX, None);
    assert_eq!(status, 200);
    let (status, _) = s.call("GET", &format!("/uploads/{upload}"), LINUX, None);
    assert_eq!(status, 404);

    // A sha256 that does not match the bytes: 422 at complete, and the session is gone.
    let (_, session) = s.call(
        "POST",
        &format!("/jobs/{job}/uploads"),
        LINUX,
        Some(json!({"name": "b.bin", "size": 1000, "sha256": sha(b"something else")})),
    );
    let upload = session["upload_id"].as_str().expect("id").to_owned();
    let (status, _) = s.chunk(&upload, LINUX, &file[..1000], 0, 1000);
    assert_eq!(status, 200);
    let (status, err) = s.call("POST", &format!("/uploads/{upload}/complete"), LINUX, None);
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (422, Some("checksum_mismatch"))
    );
    let (status, _) = s.call("GET", &format!("/uploads/{upload}"), LINUX, None);
    assert_eq!(status, 404);
    let (_, full) = s.call("GET", &format!("/jobs/{job}"), LINUX, None);
    assert_eq!(full["attachments"].as_array().map(Vec::len), Some(0));
}

#[test]
fn quota_limits_and_the_old_put() {
    let s = Server::start();
    let job = s.task();
    let (status, err) = s.start_upload(&job, "huge.bin", &data(8 * 1024 * 1024 + 1));
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (413, Some("too_large")),
        "over max_file_bytes"
    );

    let small = data(5000);
    let put = |job: &str, name: &str, bytes: &[u8]| {
        let mut resp = s
            .http
            .put(format!("{}/jobs/{job}/attachments/{name}", s.base))
            .header("Authorization", format!("Bearer {LINUX}"))
            .header("X-Sha256", sha(bytes))
            .send(bytes)
            .expect("put");
        (
            resp.status().as_u16(),
            resp.body_mut().read_json::<Value>().unwrap_or(Value::Null),
        )
    };
    let (status, att) = put(&job, "small.log", &small);
    assert_eq!(
        (status, att["size"].as_u64()),
        (200, Some(5000)),
        "the one-request PUT still works"
    );

    let (status, _) = s.start_upload(&job, "seven.bin", &data(7 * 1024 * 1024));
    assert_eq!(status, 201);
    let (_, me) = s.call("GET", "/whoami", LINUX, None);
    assert_eq!(me["quota"]["used"].as_u64(), Some(7 * 1024 * 1024 + 5000), "{me}");
    assert_eq!(me["quota"]["limit"].as_u64(), Some(12 * 1024 * 1024));
    assert_eq!(me["limits"]["upload_file_bytes"].as_u64(), Some(8 * 1024 * 1024));
    assert_eq!(me["limits"]["file_bytes"].as_u64(), Some(64 * 1024 * 1024));

    let other = s.task();
    let (status, err) = s.start_upload(&other, "six.bin", &data(6 * 1024 * 1024));
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (413, Some("over_quota")),
        "{err}"
    );
    assert!(
        err["error"]["message"].as_str().is_some_and(|m| m.contains("12582912")),
        "{err}"
    );
    let (status, err) = put(&other, "more.log", &data(6 * 1024 * 1024));
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (413, Some("over_quota")),
        "the old PUT counts too"
    );
    let (_, theirs) = s.call("GET", "/whoami", WINDOWS, None);
    assert_eq!(theirs["quota"]["used"].as_u64(), Some(0), "the recipient pays nothing");

    // Cancelling the job frees what its files and open uploads held.
    let (status, _) = s.call("POST", &format!("/jobs/{job}/cancel"), LINUX, None);
    assert_eq!(status, 200);
    let (_, me) = s.call("GET", "/whoami", LINUX, None);
    assert_eq!(me["quota"]["used"].as_u64(), Some(0));
    let (status, _) = s.start_upload(&other, "six.bin", &data(6 * 1024 * 1024));
    assert_eq!(status, 201);
}

#[test]
fn a_disk_below_its_reserve_refuses_uploads() {
    let df_works = std::process::Command::new("df")
        .args(["-P", "-k", "--"])
        .arg(std::env::temp_dir())
        .output()
        .is_ok_and(|o| o.status.success());
    if !df_works {
        eprintln!("no POSIX df here: the free-space check is skipped, as the server does");
        return;
    }
    // No disk has 8 EiB free.
    let s = Server::start_with("[quota]\ndisk_free_min_bytes = 9223372036854775807\n");
    let job = s.task();
    let (status, err) = s.start_upload(&job, "a.bin", b"abc");
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (507, Some("insufficient_storage")),
        "{err}"
    );
    let mut resp = s
        .http
        .put(format!("{}/jobs/{job}/attachments/a.txt", s.base))
        .header("Authorization", format!("Bearer {LINUX}"))
        .header("X-Sha256", sha(b"abc"))
        .send(&b"abc"[..])
        .expect("put");
    assert_eq!(resp.status().as_u16(), 507);
    let err: Value = resp.body_mut().read_json().expect("json");
    assert_eq!(err["error"]["code"].as_str(), Some("insufficient_storage"));
}
