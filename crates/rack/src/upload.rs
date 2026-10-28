//! Big files and folders. An attachment over `CHUNKED_ABOVE` goes through the
//! server's resumable upload, in chunks read from the file one at a time, so
//! neither side ever holds the whole file in memory and a cut connection costs
//! at most one chunk. The SHA-256 the server checks at the end is computed in
//! a first pass over the file, so a chunked upload reads the file twice. A
//! directory travels as one `NAME.tar`, packed into a temporary directory
//! first; a received `.tar` is unpacked by `crate::tar` after its download.

use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;

use crate::client::{AttachmentInfo, Client, RackError, Result, attachment_name, local, new_key, sha256_file};
use crate::tar;

/// Files up to this size go in one `PUT`, as before; bigger ones in chunks.
pub const CHUNKED_ABOVE: u64 = 32 * 1024 * 1024;
/// Failed chunks in a row, without progress, before the upload gives up and
/// leaves the session for `rack attach --resume`.
const ATTEMPTS: u32 = 5;
/// The largest chunk this client reads into memory, whatever the server offers.
const MAX_CHUNK: u64 = 64 * 1024 * 1024;

/// Whether an upload may start a session or must continue one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Chunked above the threshold; an open session of the same file is continued.
    Auto,
    /// Always chunked, and only an open session of the same file is continued:
    /// none is an error, so a resume never starts over by surprise.
    Resume,
}

pub struct Options<'a> {
    pub mode: Mode,
    pub chunked_above: u64,
    /// Where `FILE 512/2048 MiB` lines go, at most once per `progress_every`,
    /// and the entries a packed directory left out (symlinks, special files).
    pub progress: Option<&'a mut dyn Write>,
    pub progress_every: Duration,
}

impl Default for Options<'_> {
    fn default() -> Self {
        Self {
            mode: Mode::Auto,
            chunked_above: CHUNKED_ABOVE,
            progress: None,
            progress_every: Duration::from_secs(3),
        }
    }
}

/// An upload session as the server answers it.
#[derive(Deserialize)]
pub struct Session {
    pub upload_id: String,
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub received: u64,
    pub chunk_size: u64,
}

#[derive(Deserialize)]
struct Sessions {
    uploads: Vec<Session>,
}

#[derive(Deserialize)]
struct Completed {
    attachment: AttachmentInfo,
}

/// Writes `NAME DONE/TOTAL MiB` lines, never more often than `every`, and always the last one.
pub struct Progress<'a> {
    out: Option<&'a mut dyn Write>,
    name: String,
    total: u64,
    every: Duration,
    last: Option<Instant>,
}

impl<'a> Progress<'a> {
    pub fn new(out: Option<&'a mut dyn Write>, name: &str, total: u64, every: Duration) -> Self {
        Self {
            out,
            name: name.to_owned(),
            total,
            every,
            last: None,
        }
    }

    pub fn tick(&mut self, done: u64) {
        let due = self.last.is_none_or(|t| t.elapsed() >= self.every) || done >= self.total;
        if let (true, Some(out)) = (due, self.out.as_mut()) {
            // A progress line that cannot be written is not worth failing the upload for.
            let _ = writeln!(out, "{} {}/{} MiB", self.name, done >> 20, self.total >> 20);
            self.last = Some(Instant::now());
        }
    }
}

/// A directory under the system temp dir for one packed or downloaded
/// archive, removed with everything in it when dropped.
struct Staging(PathBuf);

impl Staging {
    fn new() -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("rack-{}", new_key()));
        fs::create_dir(&dir).map_err(|e| local(&format!("create {}", dir.display()), e))?;
        Ok(Self(dir))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Uploads `path` as an attachment of `job`: a file as itself, a directory as
/// `NAME.tar` (ustar, see `crate::tar::pack_dir`), sent like any file of that size.
pub fn upload_path(c: &Client, job: &str, path: &Path, mut opts: Options<'_>) -> Result<AttachmentInfo> {
    if !path.is_dir() {
        return upload_file(c, job, path, opts);
    }
    let name = path
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().and_then(|n| n.to_str()).map(|n| format!("{n}.tar")))
        .ok_or_else(|| RackError::Usage(format!("{} has no name to pack it under", path.display())))?;
    let staging = Staging::new()?;
    let archive = staging.0.join(&name);
    attachment_name(&archive)?;
    let mut out = BufWriter::new(File::create(&archive).map_err(|e| local(&format!("create {name}"), e))?);
    let packed = tar::pack_dir(path, &mut out).map_err(RackError::Local)?;
    out.flush().map_err(|e| local(&format!("write {name}"), e))?;
    drop(out);
    if let Some(w) = opts.progress.as_mut() {
        for skipped in &packed.skipped {
            // A warning that cannot be written is not worth failing the upload for.
            let _ = writeln!(w, "rack: {skipped}");
        }
    }
    upload_file(c, job, &archive, opts)
}

/// Downloads attachment `name` of `job`, checks its SHA-256 and unpacks it
/// into `dest` (created if missing) within `limits`. The archive goes to a
/// temporary directory, never next to the files it holds, and is deleted
/// after. Returns the verified SHA-256 and what was unpacked.
pub fn extract_attachment(
    c: &Client,
    job: &str,
    name: &str,
    dest: &Path,
    limits: &tar::Limits,
) -> Result<(String, tar::Extracted)> {
    let staging = Staging::new()?;
    let archive = staging.0.join(name);
    let got = c.download(job, name, &archive)?;
    if !got.verified() {
        return Err(RackError::Local(format!(
            "sha256 mismatch: server says {}, got {}; nothing was unpacked",
            got.expected, got.actual
        )));
    }
    fs::create_dir_all(dest).map_err(|e| local(&format!("create {}", dest.display()), e))?;
    let done =
        tar::extract(&archive, dest, limits).map_err(|e| RackError::Local(format!("{name} not unpacked: {e}")))?;
    Ok((got.actual, done))
}

/// Uploads `file` as an attachment of `job`: one `PUT` up to
/// `opts.chunked_above`, otherwise (or with `Mode::Resume`) a resumable upload.
pub fn upload_file(c: &Client, job: &str, file: &Path, opts: Options<'_>) -> Result<AttachmentInfo> {
    let name = attachment_name(file)?;
    let size = file
        .metadata()
        .map_err(|e| local(&format!("read {}", file.display()), e))?
        .len();
    if opts.mode == Mode::Auto && size <= opts.chunked_above {
        return c.upload(job, file);
    }
    chunked(c, job, file, &name, size, opts)
}

fn chunked(c: &Client, job: &str, file: &Path, name: &str, size: u64, opts: Options<'_>) -> Result<AttachmentInfo> {
    let sha = sha256_file(file)?;
    let session = match opts.mode {
        Mode::Resume => find_session(c, job, name, size, &sha)?,
        Mode::Auto => c.post(
            &format!("/jobs/{job}/uploads"),
            Some(&json!({"name": name, "size": size, "sha256": sha})),
        )?,
    };
    let id = upload_id(&session.upload_id)?;
    let chunk = session.chunk_size.clamp(1, MAX_CHUNK);
    let mut buf = vec![0u8; usize::try_from(chunk).unwrap_or(usize::MAX)];
    let mut f = File::open(file).map_err(|e| local(&format!("open {}", file.display()), e))?;
    let mut progress = Progress::new(opts.progress, name, size, opts.progress_every);
    let mut received = session.received;
    progress.tick(received);
    let mut failures = 0;
    while received < size {
        let len = chunk.min(size - received);
        let part = &mut buf[..usize::try_from(len).unwrap_or(usize::MAX)];
        f.seek(SeekFrom::Start(received))
            .and_then(|_| f.read_exact(part))
            .map_err(|e| local(&format!("read {} (did it change while sending?)", file.display()), e))?;
        let range = format!("bytes {received}-{}/{size}", received + len - 1);
        match c.put_once::<Session>(&format!("/uploads/{id}"), part, &[("Content-Range", &range)]) {
            Ok(s) => {
                failures = 0;
                received = s.received;
            }
            Err(e) if healable(&e) && failures + 1 < ATTEMPTS => {
                failures += 1;
                std::thread::sleep(Duration::from_secs(u64::from(failures)));
                // The answer to a cut chunk may never come: the server's offset is the truth.
                match c.get::<Session>(&format!("/uploads/{id}")) {
                    Ok(s) => received = s.received,
                    Err(e) if healable(&e) => {}
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(resume_hint(e, job, file)),
        }
        progress.tick(received);
    }
    let done: Completed = c
        .post(&format!("/uploads/{id}/complete"), None)
        .map_err(|e| resume_hint(e, job, file))?;
    Ok(done.attachment)
}

/// The open session for this very file, by name, size and SHA-256.
fn find_session(c: &Client, job: &str, name: &str, size: u64, sha: &str) -> Result<Session> {
    let list: Sessions = c.get(&format!("/jobs/{job}/uploads"))?;
    list.uploads
        .into_iter()
        .find(|s| s.name == name && s.size == size && s.sha256 == sha)
        .ok_or_else(|| {
            RackError::Local(format!(
                "{job} has no open upload of {name} with this size and sha256 (it completed, expired after 24 h \
                 idle, or the file changed); run `rack attach {job} FILE` without --resume to start again"
            ))
        })
}

/// A session id goes into URL paths, so it must look like the server's `u42`.
fn upload_id(id: &str) -> Result<&str> {
    let digits = id.strip_prefix('u').unwrap_or_default();
    if !digits.is_empty() && digits.len() <= 18 && digits.bytes().all(|b| b.is_ascii_digit()) {
        Ok(id)
    } else {
        Err(RackError::Transport(format!(
            "unexpected upload id `{id}` from the server"
        )))
    }
}

/// Failures a retry from the server's offset can fix: the network, a chunk that
/// raced another request, or an offset the client had wrong.
fn healable(e: &RackError) -> bool {
    match e {
        RackError::Transport(_) => true,
        RackError::Api { code, .. } => matches!(code.as_str(), "bad_offset" | "rate_limited"),
        RackError::Usage(_) | RackError::Local(_) | RackError::Unmet(_) => false,
    }
}

fn resume_hint(e: RackError, job: &str, file: &Path) -> RackError {
    if healable(&e) {
        RackError::Transport(format!(
            "{e}\nthe upload is kept on the server for 24 h: continue it with `rack attach --resume {job} {}`",
            file.display()
        ))
    } else {
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_lines_are_rate_limited_but_the_last_always_shows() {
        let mut out = Vec::new();
        {
            let mut p = Progress::new(Some(&mut out), "disk.img", 2048 << 20, Duration::from_secs(3600));
            p.tick(0);
            p.tick(512 << 20);
            p.tick(1024 << 20);
            p.tick(2048 << 20);
        }
        assert_eq!(
            String::from_utf8_lossy(&out),
            "disk.img 0/2048 MiB\ndisk.img 2048/2048 MiB\n"
        );
        let mut every = Vec::new();
        {
            let mut p = Progress::new(Some(&mut every), "a", 3 << 20, Duration::ZERO);
            for mib in 1..=3 {
                p.tick(mib << 20);
            }
        }
        assert_eq!(String::from_utf8_lossy(&every), "a 1/3 MiB\na 2/3 MiB\na 3/3 MiB\n");
        Progress::new(None, "quiet", 10, Duration::ZERO).tick(10);
    }

    #[test]
    fn upload_ids_are_checked() {
        assert!(upload_id("u12").is_ok());
        for bad in ["12", "u", "u1/../x", "u1?a=b"] {
            assert!(upload_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn what_a_retry_heals() {
        let api = |code: &str| RackError::Api {
            status: 409,
            code: code.to_owned(),
            message: String::new(),
            hint: String::new(),
        };
        assert!(healable(&RackError::Transport("reset".into())));
        assert!(healable(&api("bad_offset")) && healable(&api("rate_limited")));
        assert!(!healable(&api("not_yours")) && !healable(&api("checksum_mismatch")));
        assert!(!healable(&RackError::Local("disk".into())));
    }
}
