//! Thin blocking client over the Racking HTTP API, shared by `rack` and `racking-mcp`.

use std::fmt;
use std::fs::{self, File};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Write attempts per request when the connection fails; retries reuse the
/// `Idempotency-Key`, so the server never runs a write twice.
const ATTEMPTS: u32 = 3;

#[derive(Debug)]
pub enum RackError {
    Usage(String),
    Api {
        status: u16,
        code: String,
        message: String,
        hint: String,
    },
    Transport(String),
    Local(String),
    /// A check the caller asked for failed, so nothing was sent (`--needs` without `--force`).
    Unmet(String),
}

impl fmt::Display for RackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RackError::Usage(m) => write!(f, "{m}\n(run `rack help` for usage)"),
            RackError::Api {
                status,
                code,
                message,
                hint,
            } => write!(f, "{status} {code}: {message}\nhint: {hint}"),
            RackError::Transport(m) | RackError::Local(m) | RackError::Unmet(m) => f.write_str(m),
        }
    }
}

pub type Result<T> = std::result::Result<T, RackError>;

pub fn local(context: &str, e: impl fmt::Display) -> RackError {
    RackError::Local(format!("{context}: {e}"))
}

#[derive(Deserialize)]
pub struct Origin {
    pub repo: String,
    pub id: String,
}

#[derive(Deserialize)]
pub struct AttachmentInfo {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

/// A job as the server returns it. `body` is present on `GET /jobs/{id}`, and in
/// listings for notices and messages up to 4 KiB; other listed jobs carry
/// `body_preview`. Fields added with notices, messages and reports default, so
/// answers from an older server still parse.
#[derive(Deserialize)]
pub struct Job {
    pub id: String,
    pub from: String,
    pub to: String,
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub body_preview: Option<String>,
    #[serde(default)]
    pub body_size: u64,
    #[serde(default)]
    pub body_truncated: bool,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub expects_reply: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub origin: Option<Origin>,
    #[serde(default)]
    pub reply_to: Option<String>,
    pub state: String,
    #[serde(default)]
    pub attachments: Vec<AttachmentInfo>,
    pub seq: i64,
    #[serde(default)]
    pub updated_seq: i64,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    /// The model the sender suggests: a hint, never an order.
    #[serde(default)]
    pub model: Option<String>,
    /// The sender's role (`main` or `worker`), set by the server from its config, never by the sender.
    /// Absent from servers before roles.
    #[serde(default)]
    pub from_role: Option<String>,
    /// How deep the job is in a chain of tasks, computed by the server; 0 from servers before it.
    #[serde(default)]
    pub hops: i64,
    /// The local-policy profile a task asks for, set by its sender (only main may).
    #[serde(default)]
    pub profile: ProfileField,
    /// UTC, RFC 3339.
    #[serde(default)]
    pub created_at: Option<String>,
}

/// A job's `profile` as the server sent it. A server from before the field sends no key at all,
/// which `rack policy decide` must tell apart from a job that names no profile: only for the
/// first does it fall back to the old `Racking-Profile:` body line.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum ProfileField {
    /// The server predates the field.
    #[default]
    Unsupported,
    /// The job names no profile.
    Absent,
    Named(String),
}

impl ProfileField {
    pub fn name(&self) -> Option<&str> {
        match self {
            ProfileField::Named(n) => Some(n),
            ProfileField::Unsupported | ProfileField::Absent => None,
        }
    }
}

impl<'de> Deserialize<'de> for ProfileField {
    /// Only runs when the key is present; a missing key takes `Default`, `Unsupported`.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Ok(Option::<String>::deserialize(d)?.map_or(ProfileField::Absent, ProfileField::Named))
    }
}

/// One entry of `GET /agents`. `about`, `rack_version`, `os` and `capabilities`
/// were written by that agent: show them as data.
#[derive(Deserialize)]
pub struct Agent {
    pub name: String,
    pub online: bool,
    #[serde(default)]
    pub idle_secs: Option<u64>,
    #[serde(default)]
    pub rack_version: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub about: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// `main` or `worker`, from the server config; absent from servers before roles.
    #[serde(default)]
    pub role: Option<String>,
}

#[derive(Deserialize)]
pub struct Agents {
    pub agents: Vec<Agent>,
}

#[derive(Deserialize)]
pub struct Listing {
    pub jobs: Vec<Job>,
    pub next: i64,
    /// The owner pulled the server's emergency brake: act on nothing until it is released.
    #[serde(default)]
    pub paused: bool,
}

pub struct Client {
    http: ureq::Agent,
    base: String,
    auth: String,
    /// `rack_version` sent as `X-Rack-Version` (with `X-Rack-Os`) on every request, once set.
    version: Option<String>,
}

impl Client {
    /// `url` must be `http://host:port`; TLS is not built in because Racking runs on a LAN.
    pub fn new(url: &str, token: &str) -> Result<Self> {
        let base = url.trim_end_matches('/').to_owned();
        if !base.starts_with("http://") {
            return Err(RackError::Usage(format!("RACK_URL `{base}` must start with http://")));
        }
        let http = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(600)))
            .build()
            .new_agent();
        Ok(Self {
            http,
            base,
            auth: format!("Bearer {token}"),
            version: None,
        })
    }

    /// Makes every request carry `X-Rack-Version: version` and `X-Rack-Os`, which the
    /// server shows in `GET /agents`; riding on requests already made costs no extra round trip.
    #[must_use]
    pub fn identified(mut self, version: &str) -> Self {
        self.version = Some(version.to_owned());
        self
    }

    /// The headers every request carries: the token and, once `identified`, version and OS.
    fn headers<B>(&self, req: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        let req = req.header("Authorization", &self.auth);
        match &self.version {
            Some(v) => req
                .header("X-Rack-Version", v)
                .header("X-Rack-Os", std::env::consts::OS),
            None => req,
        }
    }

    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self.retry(|| self.headers(self.http.get(self.url(path))).call())?;
        read_json(resp)
    }

    /// POST with a JSON body (or none) and a fresh `Idempotency-Key`.
    pub fn post<T: DeserializeOwned>(&self, path: &str, body: Option<&Value>) -> Result<T> {
        let key = new_key();
        let resp = self.retry(|| {
            let req = self
                .headers(self.http.post(self.url(path)))
                .header("Idempotency-Key", &key);
            match body {
                Some(b) => req.send_json(b),
                None => req.send_empty(),
            }
        })?;
        read_json(resp)
    }

    pub fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self.retry(|| self.headers(self.http.delete(self.url(path))).call())?;
        read_json(resp)
    }

    pub fn put_json<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T> {
        let key = new_key();
        let resp = self.retry(|| {
            self.headers(self.http.put(self.url(path)))
                .header("Idempotency-Key", &key)
                .send_json(body)
        })?;
        read_json(resp)
    }

    /// Uploads `file` as attachment `name` of `job`, with its SHA-256.
    pub fn upload(&self, job: &str, file: &Path) -> Result<AttachmentInfo> {
        let name = attachment_name(file)?;
        self.put_file(&format!("/jobs/{job}/attachments/{name}"), file, &[])
    }

    /// PUTs `file` as a raw body with its `X-Sha256` and any `headers`.
    pub fn put_file<T: DeserializeOwned>(&self, path: &str, file: &Path, headers: &[(&str, &str)]) -> Result<T> {
        let sha = sha256_file(file)?;
        let key = new_key();
        let resp = self.retry(|| {
            let f = File::open(file).map_err(ureq::Error::Io)?;
            let mut req = self
                .headers(self.http.put(self.url(path)))
                .header("Idempotency-Key", &key)
                .header("X-Sha256", &sha)
                .header("Content-Type", "application/octet-stream");
            for (name, value) in headers {
                req = req.header(*name, *value);
            }
            req.send(&f)
        })?;
        read_json(resp)
    }

    /// Downloads an attachment to `dest` through a `.part` file and returns
    /// the server's `X-Sha256` and the hash of the bytes received; the caller
    /// decides what a mismatch means.
    pub fn download(&self, job: &str, name: &str, dest: &Path) -> Result<Downloaded> {
        self.download_path(&format!("/jobs/{job}/attachments/{name}"), dest, None)
    }

    /// Like `download` for any path that answers a file with `X-Sha256`. With
    /// `max_bytes`, a longer answer is cut off as an error instead of filling the disk.
    pub fn download_path(&self, path: &str, dest: &Path, max_bytes: Option<u64>) -> Result<Downloaded> {
        let mut resp = self.retry(|| self.headers(self.http.get(self.url(path))).call())?;
        if !resp.status().is_success() {
            return Err(api_error(resp));
        }
        let expected = resp
            .headers()
            .get("x-sha256")
            .and_then(|v| v.to_str().ok())
            .map(str::to_ascii_lowercase)
            .ok_or_else(|| RackError::Transport(format!("{path}: answer has no X-Sha256 header")))?;
        let part = with_suffix(dest, ".part");
        let mut out = File::create(&part).map_err(|e| local(&format!("create {}", part.display()), e))?;
        let mut hasher = Sha256::new();
        let mut reader = resp.body_mut().as_reader();
        let mut buf = vec![0u8; 64 * 1024];
        let mut size = 0u64;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(RackError::Transport(format!("{path}: download cut: {e}"))),
            };
            size += n as u64;
            if max_bytes.is_some_and(|max| size > max) {
                return Err(RackError::Transport(format!(
                    "{path}: answer is longer than the expected size"
                )));
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])
                .map_err(|e| local(&format!("write {}", part.display()), e))?;
        }
        out.sync_all()
            .map_err(|e| local(&format!("flush {}", part.display()), e))?;
        drop(out);
        fs::rename(&part, dest).map_err(|e| local(&format!("move into {}", dest.display()), e))?;
        Ok(Downloaded {
            expected,
            actual: hex(&hasher.finalize()),
            size,
        })
    }

    /// `POST /jobs`. A server from before notices, messages and reports refuses
    /// those kinds with a 400 on `kind`, and one from before the model hint
    /// refuses `model` as an unknown field; those answers get a hint saying so.
    pub fn create_job(&self, job: &Value) -> Result<Job> {
        match self.post("/jobs", Some(job)) {
            Err(RackError::Api {
                status: 400,
                code,
                message,
                ..
            }) if message.starts_with("kind:") => Err(RackError::Api {
                status: 400,
                code,
                message,
                hint: "This Racking server predates notices, messages and reports; update racking-server, \
                       or send a task instead."
                    .to_owned(),
            }),
            Err(e) => Err(older_server(
                older_server(
                    e,
                    "unknown field `model`",
                    "This Racking server predates the model hint; update racking-server, or send without a model.",
                ),
                "unknown field `profile`",
                "This Racking server predates the profile field; update racking-server, or name the profile on the \
                 first body line `Racking-Profile: NAME` (read only from main).",
            )),
            ok => ok,
        }
    }

    /// `PUT /agents/me` with the whole `capabilities` list; empty clears it.
    pub fn set_capabilities(&self, capabilities: &[String]) -> Result<Agent> {
        self.put_json("/agents/me", &serde_json::json!({ "capabilities": capabilities }))
            .map_err(|e| {
                older_server(
                    e,
                    "unknown field `capabilities`",
                    "This Racking server predates capabilities; update racking-server.",
                )
            })
    }

    /// Accept needs `received`; receiving first makes accept work on a job never read.
    pub fn accept(&self, id: &str) -> Result<Job> {
        match self.post::<Job>(&format!("/jobs/{id}/receive"), None) {
            Ok(_) | Err(RackError::Api { status: 409, .. }) => {}
            Err(e) => return Err(e),
        }
        self.post(&format!("/jobs/{id}/accept"), None)
    }

    /// Downloads to `dest` and returns the verified SHA-256. On a mismatch the
    /// file is kept as `<dest>.bad`, never under `dest`, so nobody uses it by mistake.
    pub fn download_verified(&self, job: &str, name: &str, dest: &Path) -> Result<String> {
        let got = self.download(job, name, dest)?;
        if got.verified() {
            return Ok(got.actual);
        }
        let bad = with_suffix(dest, ".bad");
        fs::rename(dest, &bad).map_err(|e| local(&format!("move to {}", bad.display()), e))?;
        Err(RackError::Local(format!(
            "sha256 mismatch: server says {}, got {}; kept as {}",
            got.expected,
            got.actual,
            bad.display()
        )))
    }

    /// The server URL, without a trailing slash.
    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn retry<F>(&self, mut send: F) -> Result<ureq::http::Response<ureq::Body>>
    where
        F: FnMut() -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    {
        let mut attempt = 1;
        loop {
            match send() {
                Ok(resp) => return Ok(resp),
                Err(e) if attempt < ATTEMPTS && is_transient(&e) => {
                    std::thread::sleep(Duration::from_secs(u64::from(attempt)));
                    attempt += 1;
                }
                Err(e) => return Err(RackError::Transport(format!("{}: {e}", self.base))),
            }
        }
    }
}

/// How a `result` job ends the job it answers.
#[derive(Clone, Copy)]
pub enum Outcome {
    Done,
    Failed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Failed => "failed",
        }
    }
}

/// The `result` job answering `original`: it goes back to the sender, and the
/// default title comes from the original one, cut to the server's 200 chars.
pub fn result_job(original: &Job, outcome: Outcome, title: Option<&str>, body: &str) -> Value {
    let title: String = match title {
        Some(t) => t.to_owned(),
        None => format!("Result: {}", original.title).chars().take(200).collect(),
    };
    serde_json::json!({
        "to": original.from,
        "kind": "result",
        "reply_to": original.id,
        "outcome": outcome.as_str(),
        "title": title,
        "body": body,
    })
}

/// Job ids go into URL paths, so they are checked before use.
pub fn job_id(id: &str) -> Result<&str> {
    let digits = id.strip_prefix('j').unwrap_or_default();
    if !digits.is_empty() && digits.len() <= 18 && digits.bytes().all(|b| b.is_ascii_digit()) {
        Ok(id)
    } else {
        Err(RackError::Usage(format!("job id `{id}` must look like j42")))
    }
}

/// An attachment name to download: it goes into the URL and becomes a local
/// file name, so a leading dot (`..`, hidden files) is refused too.
pub fn download_name(name: &str) -> Result<&str> {
    if (1..=100).contains(&name.len())
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        Ok(name)
    } else {
        Err(RackError::Usage(format!(
            "attachment name `{name}` must match [A-Za-z0-9._-]{{1,100}} and not start with a dot"
        )))
    }
}

pub struct Downloaded {
    pub expected: String,
    pub actual: String,
    pub size: u64,
}

impl Downloaded {
    pub fn verified(&self) -> bool {
        self.expected == self.actual
    }
}

/// Replaces the hint of a 400 whose message names `needle`: an older server
/// refuses a field it does not know, and its own hint cannot say why.
fn older_server(e: RackError, needle: &str, hint: &str) -> RackError {
    match e {
        RackError::Api {
            status: 400,
            code,
            message,
            ..
        } if message.contains(needle) => RackError::Api {
            status: 400,
            code,
            message,
            hint: hint.to_owned(),
        },
        other => other,
    }
}

fn is_transient(e: &ureq::Error) -> bool {
    match e {
        ureq::Error::Io(_) | ureq::Error::Timeout(_) | ureq::Error::ConnectionFailed => true,
        // `ureq::Error` is non-exhaustive; anything else (bad URL, protocol) will not heal by retrying.
        _ => false,
    }
}

fn read_json<T: DeserializeOwned>(mut resp: ureq::http::Response<ureq::Body>) -> Result<T> {
    if !resp.status().is_success() {
        return Err(api_error(resp));
    }
    resp.body_mut()
        .read_json()
        .map_err(|e| RackError::Transport(format!("unexpected answer from the server: {e}")))
}

fn api_error(mut resp: ureq::http::Response<ureq::Body>) -> RackError {
    #[derive(Deserialize)]
    struct Envelope {
        error: Inner,
    }
    #[derive(Deserialize)]
    struct Inner {
        code: String,
        message: String,
        hint: String,
    }
    let status = resp.status().as_u16();
    match resp.body_mut().read_json::<Envelope>() {
        Ok(Envelope { error }) => RackError::Api {
            status,
            code: error.code,
            message: error.message,
            hint: error.hint,
        },
        Err(e) => RackError::Transport(format!("HTTP {status} without a Racking error body: {e}")),
    }
}

/// The attachment name is the file name, which must already fit the server's
/// `[A-Za-z0-9._-]{1,100}` so no URL encoding is needed.
fn attachment_name(file: &Path) -> Result<String> {
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let ok = (1..=100).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(name.to_owned())
    } else {
        Err(RackError::Usage(format!(
            "attachment `{}`: file names must match [A-Za-z0-9._-]{{1,100}}; copy or rename it first",
            file.display()
        )))
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).map_err(|e| local(&format!("open {}", path.display()), e))?;
    let mut hasher = Sha256::new();
    io::copy(&mut f, &mut hasher).map_err(|e| local(&format!("read {}", path.display()), e))?;
    Ok(hex(&hasher.finalize()))
}

pub fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// A key unique per request: a per-process random hasher seeded with time,
/// process id and a counter. Not a secret, only a retry identity.
pub fn new_key() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let state = std::hash::RandomState::new();
    let half = |salt: u8| {
        let mut h = state.build_hasher();
        h.write_u8(salt);
        h.write_u128(nanos);
        h.write_u32(std::process::id());
        h.write_u32(n);
        h.finish()
    };
    format!("rack-{:016x}{:016x}", half(1), half(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_differ() {
        let a = new_key();
        assert_ne!(a, new_key());
        assert!(a.len() <= 80);
    }

    #[test]
    fn attachment_names() {
        assert_eq!(
            attachment_name(Path::new("dir/probe.log")).ok().as_deref(),
            Some("probe.log")
        );
        assert!(attachment_name(Path::new("my file.txt")).is_err());
        assert!(attachment_name(Path::new("ñ.txt")).is_err());
    }

    #[test]
    fn job_ids() {
        assert!(job_id("j12").is_ok());
        assert!(job_id("12").is_err());
        assert!(job_id("j1/../x").is_err());
    }

    #[test]
    fn older_servers_get_a_hint() {
        // What a server from before the model hint answers, from serde's deny_unknown_fields.
        let refused = |message: &str| RackError::Api {
            status: 400,
            code: "bad_request".to_owned(),
            message: message.to_owned(),
            hint: "See GET /help.".to_owned(),
        };
        let hint = |e: RackError| match e {
            RackError::Api { hint, .. } => hint,
            other => other.to_string(),
        };
        let old = refused("JSON body: unknown field `model`, expected one of `to`, `kind` at line 1 column 40");
        assert_eq!(hint(older_server(old, "unknown field `model`", "update")), "update");
        let other = refused("title: required for kind task");
        assert_eq!(
            hint(older_server(other, "unknown field `model`", "update")),
            "See GET /help."
        );
    }

    #[test]
    fn a_missing_profile_key_is_an_older_server() {
        let job = |extra: serde_json::Value| {
            let mut v = serde_json::json!({"id": "j1", "from": "a", "to": "b", "kind": "task", "title": "t",
                "state": "sent", "seq": 1});
            if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
                o.extend(e.clone());
            }
            serde_json::from_value::<Job>(v).map(|j| j.profile).expect("job")
        };
        assert_eq!(job(serde_json::json!({})), ProfileField::Unsupported);
        assert_eq!(job(serde_json::json!({"profile": null})), ProfileField::Absent);
        assert_eq!(
            job(serde_json::json!({"profile": "app-test"})),
            ProfileField::Named("app-test".to_owned())
        );
    }

    #[test]
    fn download_names() {
        assert!(download_name("probe.log").is_ok());
        assert!(download_name("..").is_err());
        assert!(download_name(".env").is_err());
        assert!(download_name("a/b").is_err());
        assert!(download_name("").is_err());
    }
}
