//! Chunked and resumed uploads with `rack attach`, against an in-process server
//! with 64 KiB chunks so the files stay small.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rack::client::Client;
use rack::upload::{self, Mode, Options};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const LINUX: &str = "linux-token-0123456789abcdef";
const WINDOWS: &str = "windows-token-0123456789abcdef";
const CHUNK: usize = 64 * 1024;

struct Env {
    root: PathBuf,
    url: String,
    server: Option<racking_server::Running>,
}

impl Env {
    fn new() -> Self {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("rack-attach-{}-{run}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        let config = racking_server::Config::parse(&format!(
            "bind = \"127.0.0.1:0\"\ndata_dir = {:?}\n[agents.linux]\ntoken = \"{LINUX}\"\n\
             [agents.windows]\ntoken = \"{WINDOWS}\"\n[limits]\nupload_chunk_bytes = {CHUNK}\n\
             [quota]\ndisk_free_min_bytes = 0\n",
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

    fn client(&self, token: &str) -> Client {
        Client::new(&self.url, token).expect("client")
    }

    fn rack(&self, args: &[&str]) -> Output {
        self.rack_as(LINUX, args)
    }

    /// `rack` with `token`, run in the scratch root so relative paths land there.
    fn rack_as(&self, token: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rack"))
            .args(["--state", &self.root.join("state.json").display().to_string()])
            .args(args)
            .current_dir(&self.root)
            .env("RACK_URL", &self.url)
            .env("RACK_TOKEN", token)
            .env_remove("RACK_CONFIG")
            .output()
            .expect("run rack")
    }

    fn task(&self) -> String {
        let job: Value = self
            .client(LINUX)
            .post(
                "/jobs",
                Some(&json!({"to": "windows", "title": "Image", "body": "see file"})),
            )
            .expect("job");
        job["id"].as_str().expect("id").to_owned()
    }

    fn file(&self, name: &str, len: usize) -> (PathBuf, Vec<u8>) {
        let bytes: Vec<u8> = (0..len).map(|i| (i * 31 + i / 997) as u8).collect();
        let path = self.root.join(name);
        fs::write(&path, &bytes).expect("write file");
        (path, bytes)
    }

    /// What the recipient downloads, verified against the server's sha256.
    fn fetched(&self, job: &str, name: &str) -> Vec<u8> {
        let dest = self.root.join(format!("got-{name}"));
        self.client(WINDOWS)
            .download_verified(job, name, &dest)
            .expect("download");
        fs::read(dest).expect("read download")
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

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn big_files_go_in_chunks_with_progress() {
    let env = Env::new();
    let job = env.task();
    let (path, bytes) = env.file("disk.img", 5 * CHUNK + 123);
    let mut progress = Vec::new();
    let info = upload::upload_file(
        &env.client(LINUX),
        &job,
        &path,
        Options {
            chunked_above: 1024,
            progress: Some(&mut progress),
            progress_every: Duration::ZERO,
            ..Options::default()
        },
    )
    .expect("upload");
    assert_eq!(
        (info.name.as_str(), info.size, info.sha256.as_str()),
        ("disk.img", bytes.len() as u64, sha(&bytes).as_str())
    );
    let lines = String::from_utf8_lossy(&progress);
    assert_eq!(lines.lines().count(), 7, "one line before and one per chunk: {lines}");
    assert!(lines.lines().all(|l| l == "disk.img 0/0 MiB"), "{lines}");
    assert_eq!(env.fetched(&job, "disk.img"), bytes);

    // Under the threshold the file goes in one request, as before.
    let (small, small_bytes) = env.file("small.log", 2000);
    let mut quiet = Vec::new();
    let info = upload::upload_file(
        &env.client(LINUX),
        &job,
        &small,
        Options {
            progress: Some(&mut quiet),
            ..Options::default()
        },
    )
    .expect("small upload");
    assert_eq!(info.size, 2000);
    assert!(quiet.is_empty(), "no progress for a one-request upload");
    assert_eq!(env.fetched(&job, "small.log"), small_bytes);
}

#[test]
fn a_cut_upload_resumes_from_the_server_offset() {
    let env = Env::new();
    let job = env.task();
    let (path, bytes) = env.file("trace.bin", 4 * CHUNK + 77);
    let path_text = path.display().to_string();

    let out = env.rack(&["attach", "--resume", &job, &path_text]);
    assert_eq!(out.status.code(), Some(1), "nothing to resume yet");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no open upload of trace.bin"));

    // A client that sent one chunk and a half before its connection dropped.
    let c = env.client(LINUX);
    let session: Value = c
        .post(
            &format!("/jobs/{job}/uploads"),
            Some(&json!({"name": "trace.bin", "size": bytes.len(), "sha256": sha(&bytes)})),
        )
        .expect("start");
    let id = session["upload_id"].as_str().expect("id");
    let first: Value = c
        .put_once(
            &format!("/uploads/{id}"),
            &bytes[..CHUNK],
            &[("Content-Range", &format!("bytes 0-{}/{}", CHUNK - 1, bytes.len()))],
        )
        .expect("first chunk");
    assert_eq!(first["received"].as_u64(), Some(CHUNK as u64));

    let out = env.rack(&["attach", "--resume", &job, &path_text]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        stdout.trim(),
        format!(
            "{job} attached trace.bin ({} bytes, sha256 {})",
            bytes.len(),
            sha(&bytes)
        )
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("trace.bin 0/0 MiB"),
        "progress goes to stderr"
    );
    assert_eq!(env.fetched(&job, "trace.bin"), bytes);
    let left: Value = c.get(&format!("/jobs/{job}/uploads")).expect("list");
    assert_eq!(
        left["uploads"].as_array().map(Vec::len),
        Some(0),
        "the session completed"
    );
}

#[test]
fn a_plain_attach_continues_an_open_session_of_the_same_file() {
    let env = Env::new();
    let job = env.task();
    let (path, bytes) = env.file("core.dump", 3 * CHUNK);
    let c = env.client(LINUX);
    let session: Value = c
        .post(
            &format!("/jobs/{job}/uploads"),
            Some(&json!({"name": "core.dump", "size": bytes.len(), "sha256": sha(&bytes)})),
        )
        .expect("start");
    let id = session["upload_id"].as_str().expect("id").to_owned();
    c.put_once::<Value>(
        &format!("/uploads/{id}"),
        &bytes[..CHUNK],
        &[("Content-Range", &format!("bytes 0-{}/{}", CHUNK - 1, bytes.len()))],
    )
    .expect("chunk");
    let info = upload::upload_file(
        &c,
        &job,
        &path,
        Options {
            mode: Mode::Auto,
            chunked_above: 0,
            ..Options::default()
        },
    )
    .expect("continue");
    assert_eq!(info.sha256, sha(&bytes));
    let done: Value = c.get(&format!("/uploads/{id}")).expect("session");
    assert_eq!(
        done["state"].as_str(),
        Some("complete"),
        "the same session was finished"
    );
}

#[test]
fn a_directory_travels_as_a_tar_and_unpacks_safely() {
    let env = Env::new();
    let job = env.task();
    let src = env.root.join("traces");
    fs::create_dir_all(src.join("gpu")).expect("tree");
    fs::write(src.join("summary.md"), b"# two devices\n").expect("file");
    fs::write(src.join("gpu/frame-001.bin"), vec![9u8; 70_000]).expect("file");

    let out = env.rack(&["attach", &job, "traces"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.starts_with(&format!("{job} attached traces.tar (")), "{stdout}");

    let out = env.rack_as(
        WINDOWS,
        &["get-attachment", &job, "traces.tar", "--extract", "received"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        stdout.starts_with("traces.tar extracted into received: 2 files, 2 dirs, 70014 bytes (sha256 "),
        "{stdout}"
    );
    let got = env.root.join("received/traces");
    assert_eq!(
        fs::read(got.join("summary.md")).ok().as_deref(),
        Some(&b"# two devices\n"[..])
    );
    assert_eq!(fs::read(got.join("gpu/frame-001.bin")).ok(), Some(vec![9u8; 70_000]));
    assert!(!env.root.join("traces.tar").exists(), "the archive is not left behind");

    // A second unpack meets the files of the first: refused, nothing replaced, unless --overwrite.
    fs::write(got.join("summary.md"), b"edited\n").expect("edit");
    let out = env.rack_as(
        WINDOWS,
        &["get-attachment", &job, "traces.tar", "--extract", "received"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--overwrite"));
    assert_eq!(fs::read(got.join("summary.md")).ok().as_deref(), Some(&b"edited\n"[..]));
    let out = env.rack_as(
        WINDOWS,
        &[
            "get-attachment",
            &job,
            "traces.tar",
            "--extract",
            "received",
            "--overwrite",
        ],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        fs::read(got.join("summary.md")).ok().as_deref(),
        Some(&b"# two devices\n"[..])
    );

    let out = env.rack_as(
        WINDOWS,
        &[
            "get-attachment",
            &job,
            "traces.tar",
            "--extract",
            "small",
            "--max-bytes",
            "100",
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--max-bytes"));
    let out = env.rack_as(WINDOWS, &["get-attachment", &job, "traces.tar", "--overwrite"]);
    assert_eq!(out.status.code(), Some(2), "--overwrite goes with --extract");
    let out = env.rack_as(
        WINDOWS,
        &["get-attachment", &job, "traces.tar", "--extract", "x", "--out", "y"],
    );
    assert_eq!(out.status.code(), Some(2));
}
