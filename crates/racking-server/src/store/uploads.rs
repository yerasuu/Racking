//! Resumable uploads: a session per file, filled chunk by chunk in order into a
//! part file under the data directory's `tmp/`, then checked whole (size and
//! SHA-256, streamed) and moved in as an attachment. Also the per-agent quota,
//! the free-space reserve, and the cleanup of expired sessions and orphan files.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Attachment, Store, View, check_upload, limits, load_job, not_yours};
use crate::clock;
use crate::config::{Quota, SenderLimits};
use crate::error::{ApiError, ApiResult, Code};
use crate::types::{AgentName, AttachmentName, ContentRange, JobId, Sha256Hex, UploadId};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS uploads (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job INTEGER NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    agent TEXT NOT NULL,
    name TEXT NOT NULL,
    size INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    content_type TEXT,
    received INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'open',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS uploads_job ON uploads (job, name);
CREATE INDEX IF NOT EXISTS uploads_agent ON uploads (agent, state);
";

const COLUMNS: &str = "id, job, agent, name, size, sha256, content_type, received, state, updated_at";

/// Reads the free bytes of the file system that holds a directory; `None` when unknown.
pub type FreeSpace = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;

/// The size limits, quota and reserve every upload is checked against, from the config.
#[derive(Clone, Copy, Debug)]
pub struct UploadRules {
    pub max_file_bytes: u64,
    pub max_job_bytes: u64,
    pub chunk_bytes: u64,
    pub per_agent_bytes: u64,
    pub disk_free_min_bytes: u64,
}

impl UploadRules {
    pub fn new(limits: &SenderLimits, quota: &Quota) -> Self {
        Self {
            max_file_bytes: limits.max_file_bytes,
            max_job_bytes: limits.max_job_bytes,
            chunk_bytes: limits.upload_chunk_bytes,
            per_agent_bytes: quota.per_agent_bytes,
            disk_free_min_bytes: quota.disk_free_min_bytes,
        }
    }
}

impl Default for UploadRules {
    fn default() -> Self {
        Self::new(&SenderLimits::default(), &Quota::default())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadState {
    Open,
    Complete,
}

impl UploadState {
    fn as_str(self) -> &'static str {
        match self {
            UploadState::Open => "open",
            UploadState::Complete => "complete",
        }
    }
}

pub struct Session {
    pub id: UploadId,
    pub job: JobId,
    agent: String,
    pub name: String,
    pub size: u64,
    pub sha256: String,
    content_type: Option<String>,
    pub received: u64,
    pub state: UploadState,
    updated_at: i64,
}

impl Session {
    pub fn to_json(&self, chunk_size: u64) -> Value {
        json!({
            "upload_id": self.id.to_string(),
            "job": self.job.to_string(),
            "name": self.name,
            "size": self.size,
            "sha256": self.sha256,
            "received": self.received,
            "state": self.state.as_str(),
            "chunk_size": chunk_size,
            "expires_at": clock::rfc3339(self.updated_at + limits::UPLOAD_IDLE_SECS),
        })
    }

    fn check_owner(&self, me: &AgentName) -> ApiResult<()> {
        if self.agent == me.as_str() {
            Ok(())
        } else {
            Err(ApiError::new(
                Code::NotYours,
                format!("upload {} belongs to another agent", self.id),
                "Only the agent that started an upload may see, fill, complete or abort it.",
            ))
        }
    }

    fn check_open(&self) -> ApiResult<()> {
        match self.state {
            UploadState::Open => Ok(()),
            UploadState::Complete => Err(ApiError::new(
                Code::Conflict,
                format!("upload {} is complete already", self.id),
                format!(
                    "The file is attachment {} of {}; start a new upload to replace it.",
                    self.name, self.job
                ),
            )),
        }
    }
}

pub struct NewUpload {
    pub name: AttachmentName,
    pub size: u64,
    pub sha256: Sha256Hex,
    pub content_type: Option<String>,
}

/// Whether `create_upload` started a session or handed back the open one for the same file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opened {
    Created,
    Resumed,
}

/// Removes the session id from the set of sessions being written when dropped,
/// so an early return cannot leave a session locked.
struct Writing<'a> {
    set: &'a Mutex<HashSet<i64>>,
    id: i64,
}

impl Drop for Writing<'_> {
    fn drop(&mut self) {
        self.set.lock().unwrap_or_else(PoisonError::into_inner).remove(&self.id);
    }
}

/// What the hourly purge removed from the database; the files go after it.
pub(super) struct Purged {
    pub jobs: Vec<JobId>,
    pub uploads: Vec<i64>,
}

impl Store {
    pub fn upload_rules(&self) -> UploadRules {
        self.rules
    }

    #[must_use]
    pub fn with_upload_rules(mut self, rules: UploadRules) -> Self {
        self.rules = rules;
        self
    }

    /// Replaces the free-space reader, so tests can play a full disk.
    #[cfg(test)]
    #[must_use]
    pub fn with_free_space(mut self, free_space: FreeSpace) -> Self {
        self.free_space = free_space;
        self
    }

    pub(super) fn part_path(&self, id: i64) -> PathBuf {
        self.tmp.join(format!("upload-{id}.part"))
    }

    /// Refuses an upload of `incoming` bytes that would leave the data
    /// directory's file system under `disk_free_min_bytes`. An unknown free
    /// space lets the upload through: the quota still bounds it.
    pub fn check_disk(&self, incoming: u64) -> ApiResult<()> {
        let reserve = self.rules.disk_free_min_bytes;
        if reserve == 0 {
            return Ok(());
        }
        let Some(free) = (self.free_space)(&self.data_dir) else {
            if !self.free_space_warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(target: "racking::store", dir = %self.data_dir.display(), "cannot read the free disk space; the disk_free_min_bytes check is skipped");
            }
            return Ok(());
        };
        if free >= reserve.saturating_add(incoming) {
            return Ok(());
        }
        tracing::warn!(target: "racking::store", free, reserve, incoming, "refused an upload: disk below its free-space reserve");
        Err(ApiError::new(
            Code::InsufficientStorage,
            format!("the server keeps {reserve} bytes free and has {free}; {incoming} more do not fit"),
            "Retry later, send a smaller file, or ask the owner to free disk space on the server.",
        ))
    }

    /// Bytes counted against `me`'s quota: the attachments of jobs it sent and
    /// the declared size of its open uploads. `except` leaves out one name of one
    /// job, the file an upload is about to replace.
    pub(super) fn quota_used_except(
        conn: &Connection,
        me: &AgentName,
        except: Option<(JobId, &str)>,
    ) -> ApiResult<u64> {
        let (job, name) = except.map_or((0, ""), |(j, n)| (j.get(), n));
        let used: i64 = conn.query_row(
            "SELECT (SELECT COALESCE(SUM(a.size), 0) FROM attachments a JOIN jobs j ON j.id = a.job \
                     WHERE j.from_agent = ?1 AND NOT (a.job = ?2 AND a.name = ?3)) \
                  + (SELECT COALESCE(SUM(size), 0) FROM uploads \
                     WHERE agent = ?1 AND state = 'open' AND NOT (job = ?2 AND name = ?3))",
            params![me.as_str(), job, name],
            |row| row.get(0),
        )?;
        Ok(used.unsigned_abs())
    }

    pub fn quota_used(&self, me: &AgentName) -> ApiResult<u64> {
        Self::quota_used_except(&self.db(), me, None)
    }

    /// Starts an upload session, or hands back the open one for the same file
    /// (same job, name, size and SHA-256), which is how a client resumes.
    pub fn create_upload(&self, me: &AgentName, id: JobId, new: NewUpload) -> ApiResult<(Session, Opened)> {
        if new.size > self.rules.max_file_bytes {
            return Err(ApiError::new(
                Code::TooLarge,
                format!(
                    "file is {} bytes, over the {} bytes one upload may carry",
                    new.size, self.rules.max_file_bytes
                ),
                "Split it (an archive in parts) or send it on several jobs.",
            ));
        }
        self.check_disk(new.size)?;
        let mut conn = self.db();
        let tx = conn.transaction()?;
        let job = load_job(&tx, id, View::Summary)?;
        if let Some(open) = open_session(&tx, id, new.name.as_str())? {
            open.check_owner(me)?;
            if open.size == new.size && open.sha256 == new.sha256.as_str() {
                return Ok((open, Opened::Resumed));
            }
            return Err(ApiError::new(
                Code::Conflict,
                format!(
                    "upload {} of {} to {id} is open with another size or sha256",
                    open.id, open.name
                ),
                format!(
                    "Resume it with the same file, or abort it first: DELETE /uploads/{}.",
                    open.id
                ),
            ));
        }
        let budget = check_upload(&tx, &self.rules, &job, me, &new.name, self.rules.max_file_bytes)?;
        if new.size > budget.max_bytes {
            return Err(budget.error());
        }
        let now = clock::now();
        let size = i64::try_from(new.size).map_err(|e| ApiError::internal("record the upload size", &e))?;
        tx.execute(
            "INSERT INTO uploads (job, agent, name, size, sha256, content_type, received, state, created_at, \
             updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 'open', ?7, ?7)",
            params![
                id.get(),
                me.as_str(),
                new.name.as_str(),
                size,
                new.sha256.as_str(),
                new.content_type,
                now
            ],
        )?;
        let upload = tx.last_insert_rowid();
        File::create(self.part_path(upload)).map_err(|e| ApiError::internal("create the upload file", &e))?;
        tx.commit()?;
        tracing::info!(target: "racking::store", agent = %me, job = %id, upload, size = new.size, "upload started");
        Ok((load_session(&conn, UploadId::new(upload))?, Opened::Created))
    }

    pub fn upload(&self, me: &AgentName, id: UploadId) -> ApiResult<Session> {
        let session = load_session(&self.db(), id)?;
        session.check_owner(me)?;
        Ok(session)
    }

    /// The caller's open uploads to job `id`, so a client that lost its session id finds it again.
    pub fn uploads_of(&self, me: &AgentName, id: JobId) -> ApiResult<Vec<Session>> {
        let conn = self.db();
        let job = load_job(&conn, id, View::Summary)?;
        if !job.is_party(me) {
            return Err(not_yours(&job));
        }
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM uploads WHERE job = ?1 AND agent = ?2 AND state = 'open' ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![id.get(), me.as_str()], session_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Writes one chunk at `range.first`, which must be the session's `received`
    /// offset. A chunk that lies wholly before it was stored already (a retry
    /// whose answer was lost) and is answered with the session unchanged, its
    /// bytes unchecked: the SHA-256 at `complete` covers every byte anyway. A
    /// chunk cut short keeps the bytes that arrived, so the retry starts there.
    pub fn put_chunk(
        &self,
        me: &AgentName,
        id: UploadId,
        range: ContentRange,
        body: &mut dyn Read,
    ) -> ApiResult<Session> {
        let session = self.upload(me, id)?;
        session.check_open()?;
        if range.total != session.size {
            return Err(ApiError::bad_request(format!(
                "Content-Range total {} differs from the upload's size {}",
                range.total, session.size
            )));
        }
        if range.length() > self.rules.chunk_bytes {
            return Err(ApiError::new(
                Code::TooLarge,
                format!(
                    "chunk of {} bytes is over the {} a chunk may carry",
                    range.length(),
                    self.rules.chunk_bytes
                ),
                format!("Send chunks of at most chunk_size ({}) bytes.", self.rules.chunk_bytes),
            ));
        }
        if range.last < session.received {
            return Ok(session);
        }
        if range.first != session.received {
            return Err(bad_offset(&session));
        }
        let _writing = self.start_writing(id)?;
        // Read again under the guard: a chunk that finished between the first read and the guard moved the offset.
        let session = self.upload(me, id)?;
        session.check_open()?;
        if range.first != session.received {
            return Err(bad_offset(&session));
        }
        let path = self.part_path(id.get());
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| ApiError::internal("open the upload file", &e))?;
        let on_disk = file
            .metadata()
            .map_err(|e| ApiError::internal("read the upload file", &e))?
            .len();
        if on_disk < range.first {
            // The part file lost bytes the session counted (a crash before they reached the disk).
            self.set_received(id, on_disk)?;
            return Err(bad_offset(&Session {
                received: on_disk,
                ..session
            }));
        }
        // Bytes past `received` are what a cut chunk left; they are rewritten now.
        file.set_len(range.first)
            .and_then(|()| file.seek(SeekFrom::Start(range.first)).map(drop))
            .map_err(|e| ApiError::internal("prepare the upload file", &e))?;
        let written = copy_chunk(body, &mut file, range.length())?;
        file.sync_data()
            .map_err(|e| ApiError::internal("flush the upload", &e))?;
        let received = range.first + written;
        self.set_received(id, received)?;
        if written < range.length() {
            return Err(ApiError::new(
                Code::BadRequest,
                format!("chunk cut short: {written} of {} bytes arrived", range.length()),
                format!("The bytes that arrived are kept: continue from offset {received}."),
            )
            .with_received(received));
        }
        self.upload(me, id)
    }

    fn set_received(&self, id: UploadId, received: u64) -> ApiResult<()> {
        let received = i64::try_from(received).map_err(|e| ApiError::internal("record the upload offset", &e))?;
        self.db().execute(
            "UPDATE uploads SET received = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.get(), received, clock::now()],
        )?;
        Ok(())
    }

    /// Marks the session as being written, so a second request for it (a client
    /// retrying while the first is still being read) cannot write the same bytes.
    fn start_writing(&self, id: UploadId) -> ApiResult<Writing<'_>> {
        let mut set = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        if !set.insert(id.get()) {
            return Err(ApiError::new(
                Code::RateLimited,
                format!("another request is writing upload {id}"),
                "Retry-After: 1, then GET the upload and continue from `received`.",
            )
            .with_retry_after(1));
        }
        Ok(Writing {
            set: &self.writing,
            id: id.get(),
        })
    }

    /// Checks size and SHA-256 of the whole file, streaming it, and makes it an
    /// attachment of the job, which must still be `sent`. Completing a session
    /// that is complete answers the same attachment again, so a retry is safe.
    pub fn complete_upload(&self, me: &AgentName, id: UploadId) -> ApiResult<(Session, Attachment)> {
        let session = self.upload(me, id)?;
        if session.state == UploadState::Complete {
            let attachment = Attachment {
                name: session.name.clone(),
                size: i64::try_from(session.size).unwrap_or(i64::MAX),
                sha256: session.sha256.clone(),
            };
            return Ok((session, attachment));
        }
        if session.received != session.size {
            return Err(ApiError::new(
                Code::BadOffset,
                format!("upload {id} has {} of {} bytes", session.received, session.size),
                format!(
                    "Send the rest from offset {} with PUT /uploads/{id}, then complete.",
                    session.received
                ),
            )
            .with_received(session.received));
        }
        let _writing = self.start_writing(id)?;
        let path = self.part_path(id.get());
        let (size, actual) = hash_file(&path)?;
        if size != session.size || actual.as_str() != session.sha256 {
            self.drop_session(id)?;
            return Err(ApiError::new(
                Code::ChecksumMismatch,
                format!(
                    "upload {id} said sha256 {} and {} bytes; the bytes stored are {size} and hash to {}",
                    session.sha256,
                    session.size,
                    actual.as_str()
                ),
                "The session is gone: check the file did not change while it was sent, then start a new upload.",
            ));
        }
        let name: AttachmentName = session
            .name
            .parse()
            .map_err(|e: String| ApiError::internal("read the upload name", &e))?;
        let mut conn = self.db();
        let tx = conn.transaction()?;
        let job = load_job(&tx, session.job, View::Summary)?;
        let budget = check_upload(&tx, &self.rules, &job, me, &name, self.rules.max_file_bytes)?;
        if size > budget.max_bytes {
            return Err(budget.error());
        }
        let dir = self.job_dir(session.job);
        fs::create_dir_all(&dir).map_err(|e| ApiError::internal("create the attachment directory", &e))?;
        fs::rename(&path, dir.join(name.as_str())).map_err(|e| ApiError::internal("store the attachment", &e))?;
        let size = i64::try_from(size).map_err(|e| ApiError::internal("record the attachment size", &e))?;
        tx.execute(
            "INSERT INTO attachments (job, name, size, sha256, content_type) VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (job, name) DO UPDATE SET size = ?3, sha256 = ?4, content_type = ?5",
            params![
                session.job.get(),
                name.as_str(),
                size,
                session.sha256,
                session.content_type
            ],
        )?;
        tx.execute(
            "UPDATE uploads SET state = 'complete', updated_at = ?2 WHERE id = ?1",
            params![id.get(), clock::now()],
        )?;
        tx.commit()?;
        drop(conn);
        self.changes.bump();
        tracing::info!(target: "racking::store", agent = %me, job = %session.job, upload = id.get(), size, "upload complete");
        let attachment = Attachment {
            name: session.name.clone(),
            size,
            sha256: session.sha256.clone(),
        };
        Ok((
            Session {
                state: UploadState::Complete,
                ..session
            },
            attachment,
        ))
    }

    /// Ends a session and deletes its data; on a complete one only the record goes.
    pub fn abort_upload(&self, me: &AgentName, id: UploadId) -> ApiResult<()> {
        self.upload(me, id)?;
        let _writing = self.start_writing(id)?;
        self.drop_session(id)
    }

    fn drop_session(&self, id: UploadId) -> ApiResult<()> {
        self.db().execute("DELETE FROM uploads WHERE id = ?1", [id.get()])?;
        self.remove_part(id.get());
        Ok(())
    }

    pub(super) fn remove_part(&self, id: i64) {
        let path = self.part_path(id);
        if let Err(e) = fs::remove_file(&path)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(target: "racking::store", path = %path.display(), error = %e, "could not remove an upload file");
        }
    }

    /// The upload sessions of job `id`, read before the job is deleted so their files can go with it.
    pub(super) fn upload_ids_of_job(conn: &Connection, id: JobId) -> ApiResult<Vec<i64>> {
        let mut stmt = conn.prepare_cached("SELECT id FROM uploads WHERE job = ?1")?;
        let rows = stmt.query_map([id.get()], |row| row.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Deletes the files nothing in the database points to any more, older than
    /// `min_age` so a file whose row is being written right now is left alone:
    /// part files of sessions that are gone, temp files of uploads a crash cut
    /// off, and attachment directories of jobs deleted without their files.
    pub(super) fn sweep_orphans(&self, min_age: Duration) {
        let live = {
            let conn = self.db();
            let ids = |sql: &str| -> rusqlite::Result<HashSet<i64>> {
                let mut stmt = conn.prepare(sql)?;
                stmt.query_map([], |row| row.get(0))?.collect()
            };
            match (
                ids("SELECT id FROM uploads WHERE state = 'open'"),
                ids("SELECT id FROM jobs"),
            ) {
                (Ok(uploads), Ok(jobs)) => (uploads, jobs),
                (Err(e), _) | (_, Err(e)) => {
                    tracing::warn!(target: "racking::store", error = %e, "orphan sweep skipped");
                    return;
                }
            }
        };
        let (uploads, jobs) = live;
        let mut removed = 0usize;
        for (path, name) in old_entries(&self.tmp, min_age) {
            let live = name
                .strip_prefix("upload-")
                .and_then(|n| n.strip_suffix(".part"))
                .and_then(|n| n.parse::<i64>().ok())
                .is_some_and(|id| uploads.contains(&id));
            if !live && remove_any(&path) {
                removed += 1;
            }
        }
        for (path, name) in old_entries(&self.files, min_age) {
            let live = name.parse::<JobId>().is_ok_and(|id| jobs.contains(&id.get()));
            if !live && remove_any(&path) {
                removed += 1;
            }
        }
        if removed > 0 {
            tracing::info!(target: "racking::store", removed, "removed orphan upload and attachment files");
        }
    }
}

/// The entries of `dir` last modified at least `min_age` ago, with their names.
fn old_entries(dir: &Path, min_age: Duration) -> Vec<(PathBuf, String)> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    read.filter_map(Result::ok)
        .filter(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| now.duration_since(t).unwrap_or_default() >= min_age)
        })
        .filter_map(|e| Some((e.path(), e.file_name().into_string().ok()?)))
        .collect()
}

fn remove_any(path: &Path) -> bool {
    let result = if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(target: "racking::store", path = %path.display(), error = %e, "could not remove an orphan file");
            false
        }
    }
}

fn bad_offset(session: &Session) -> ApiError {
    ApiError::new(
        Code::BadOffset,
        format!(
            "upload {} has {} bytes; a chunk must start there",
            session.id, session.received
        ),
        format!(
            "Send `Content-Range: bytes {}-LAST/{}` next; GET /uploads/{} shows the offset.",
            session.received, session.size, session.id
        ),
    )
    .with_received(session.received)
}

/// Copies up to `len` bytes; a body that ends early or fails to read is a cut
/// connection, so the count of bytes written comes back instead of an error.
fn copy_chunk(body: &mut dyn Read, file: &mut File, len: u64) -> ApiResult<u64> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    while written < len {
        let want = usize::try_from(len - written).map_or(buf.len(), |left| left.min(buf.len()));
        let n = match body.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::debug!(target: "racking::store", error = %e, "upload chunk cut");
                break;
            }
        };
        file.write_all(&buf[..n])
            .map_err(|e| ApiError::internal("write the upload", &e))?;
        written += n as u64;
    }
    Ok(written)
}

fn hash_file(path: &Path) -> ApiResult<(u64, Sha256Hex)> {
    let mut file = File::open(path).map_err(|e| ApiError::internal("open the upload file", &e))?;
    let mut hasher = Sha256::new();
    let size = io::copy(&mut file, &mut hasher).map_err(|e| ApiError::internal("read the upload file", &e))?;
    Ok((size, Sha256Hex::of_digest(&hasher.finalize())))
}

fn load_session(conn: &Connection, id: UploadId) -> ApiResult<Session> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM uploads WHERE id = ?1"),
        [id.get()],
        session_from_row,
    )
    .optional()?
    .ok_or_else(|| {
        ApiError::new(
            Code::NotFound,
            format!("upload {id} not found"),
            "Sessions end 24 h after their last chunk, when aborted, or when their job is cancelled; \
             GET /jobs/{id}/uploads lists yours.",
        )
    })
}

fn open_session(conn: &Connection, job: JobId, name: &str) -> ApiResult<Option<Session>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM uploads WHERE job = ?1 AND name = ?2 AND state = 'open'"),
            params![job.get(), name],
            session_from_row,
        )
        .optional()?)
}

/// Open sessions of job `id` for other names than `name`: files the job will
/// hold once they complete, so they count against its file and byte limits now.
pub(super) fn pending_on_job(conn: &Connection, id: JobId, name: &str) -> ApiResult<(usize, u64)> {
    let (count, bytes): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM uploads WHERE job = ?1 AND name <> ?2 AND state = 'open'",
        params![id.get(), name],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((usize::try_from(count).unwrap_or(usize::MAX), bytes.unsigned_abs()))
}

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<Session> {
    let state: String = row.get(8)?;
    Ok(Session {
        id: UploadId::new(row.get(0)?),
        job: JobId::new(row.get(1)?),
        agent: row.get(2)?,
        name: row.get(3)?,
        size: row.get::<_, i64>(4)?.unsigned_abs(),
        sha256: row.get(5)?,
        content_type: row.get(6)?,
        received: row.get::<_, i64>(7)?.unsigned_abs(),
        state: if state == "complete" {
            UploadState::Complete
        } else {
            UploadState::Open
        },
        updated_at: row.get(9)?,
    })
}

/// Free bytes for unprivileged users on the file system that holds `dir`, from
/// `df -P -k`, whose output format POSIX fixes
/// ([POSIX: df](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/df.html)):
/// a header, then `name total used available capacity% mount` in 1024-byte
/// units. The standard library has no call for it and `statvfs` would need
/// `unsafe` or a new dependency, so the server asks `df`; where `df` is missing
/// (Windows) this is `None` and the check is skipped.
pub fn df_free_space(dir: &Path) -> Option<u64> {
    let out = Command::new("df").args(["-P", "-k", "--"]).arg(dir).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_df(&String::from_utf8_lossy(&out.stdout))
}

fn parse_df(text: &str) -> Option<u64> {
    let fields: Vec<&str> = text.lines().nth(1)?.split_whitespace().collect();
    // The file system name may hold spaces, so find the capacity column by its `%`
    // and take the one before it; it is at least the fifth field.
    let capacity = fields.iter().skip(4).position(|f| f.ends_with('%'))? + 4;
    fields[capacity - 1].parse::<u64>().ok()?.checked_mul(1024)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::store::Kind;
    use crate::store::tests::{TempDir, agent, delivery, store, task};

    fn sha(bytes: &[u8]) -> Sha256Hex {
        Sha256Hex::of_digest(&Sha256::digest(bytes))
    }

    fn small_rules() -> UploadRules {
        UploadRules {
            max_file_bytes: 1000,
            max_job_bytes: 1500,
            chunk_bytes: 100,
            per_agent_bytes: 2500,
            disk_free_min_bytes: 0,
        }
    }

    fn small() -> (TempDir, Store) {
        let (dir, s) = store();
        (dir, s.with_upload_rules(small_rules()))
    }

    fn new_upload(name: &str, bytes: &[u8]) -> NewUpload {
        NewUpload {
            name: name.parse().expect("name"),
            size: bytes.len() as u64,
            sha256: sha(bytes),
            content_type: None,
        }
    }

    fn range(first: u64, last: u64, total: u64) -> ContentRange {
        format!("bytes {first}-{last}/{total}").parse().expect("range")
    }

    /// Sends `bytes` in chunks of `chunk` and returns the session after the last.
    fn fill(s: &Store, me: &AgentName, id: UploadId, bytes: &[u8], chunk: usize) -> Session {
        let total = bytes.len() as u64;
        let mut last = None;
        for (i, part) in bytes.chunks(chunk).enumerate() {
            let first = (i * chunk) as u64;
            let r = range(first, first + part.len() as u64 - 1, total);
            last = Some(s.put_chunk(me, id, r, &mut &part[..]).expect("chunk"));
        }
        last.expect("at least one chunk")
    }

    #[test]
    fn session_state_machine() {
        let (_d, s) = small();
        let linux = agent("linux");
        let job = s.create_job(&linux, task("windows")).expect("job");
        let bytes: Vec<u8> = (0..250u32).map(|i| (i % 251) as u8).collect();
        let (session, opened) = s
            .create_upload(&linux, job.id, new_upload("big.bin", &bytes))
            .expect("create");
        assert_eq!(
            (opened, session.received, session.state),
            (Opened::Created, 0, UploadState::Open)
        );
        assert!(s.part_path(session.id.get()).exists());

        let (again, opened) = s
            .create_upload(&linux, job.id, new_upload("big.bin", &bytes))
            .expect("resume");
        assert_eq!(
            (opened, again.id),
            (Opened::Resumed, session.id),
            "same file finds the open session"
        );
        let other = s.create_upload(&linux, job.id, new_upload("big.bin", b"other"));
        assert_eq!(other.err().map(|e| e.code), Some(Code::Conflict));

        let early = s.complete_upload(&linux, session.id).err().expect("incomplete");
        assert_eq!((early.code, early.received), (Code::BadOffset, Some(0)));

        let after = s
            .put_chunk(&linux, session.id, range(0, 99, 250), &mut &bytes[..100])
            .expect("first chunk");
        assert_eq!(after.received, 100);
        let ahead = s
            .put_chunk(&linux, session.id, range(200, 249, 250), &mut &bytes[200..])
            .err()
            .expect("gap");
        assert_eq!((ahead.code, ahead.received), (Code::BadOffset, Some(100)));
        let repeat = s
            .put_chunk(&linux, session.id, range(0, 99, 250), &mut &bytes[..100])
            .expect("a chunk already stored is a no-op");
        assert_eq!(repeat.received, 100);
        let straddle = s
            .put_chunk(&linux, session.id, range(50, 149, 250), &mut &bytes[50..150])
            .err()
            .expect("straddles the offset");
        assert_eq!(straddle.code, Code::BadOffset);
        let wrong_total = s.put_chunk(&linux, session.id, range(100, 199, 251), &mut &bytes[100..200]);
        assert_eq!(wrong_total.err().map(|e| e.code), Some(Code::BadRequest));
        let too_big = s.put_chunk(&linux, session.id, range(100, 249, 250), &mut &bytes[100..]);
        assert_eq!(too_big.err().map(|e| e.code), Some(Code::TooLarge));

        s.put_chunk(&linux, session.id, range(100, 199, 250), &mut &bytes[100..200])
            .expect("second");
        s.put_chunk(&linux, session.id, range(200, 249, 250), &mut &bytes[200..])
            .expect("third");
        let (done, att) = s.complete_upload(&linux, session.id).expect("complete");
        assert_eq!(
            (done.state, att.size, att.sha256.as_str()),
            (UploadState::Complete, 250, sha(&bytes).as_str())
        );
        assert!(
            !s.part_path(session.id.get()).exists(),
            "the part file became the attachment"
        );
        let (_, again) = s.complete_upload(&linux, session.id).expect("complete is idempotent");
        assert_eq!(again.size, 250);
        let late = s.put_chunk(&linux, session.id, range(0, 9, 250), &mut &bytes[..10]);
        assert_eq!(late.err().map(|e| e.code), Some(Code::Conflict));

        let windows = agent("windows");
        let mut dl = s
            .get_attachment(&windows, job.id, &"big.bin".parse().expect("name"))
            .expect("download");
        let mut got = Vec::new();
        dl.file.read_to_end(&mut got).expect("read");
        assert_eq!(got, bytes);
    }

    #[test]
    fn a_cut_chunk_keeps_what_arrived() {
        let (_d, s) = small();
        let linux = agent("linux");
        let job = s.create_job(&linux, task("windows")).expect("job");
        let bytes = vec![7u8; 150];
        let (session, _) = s
            .create_upload(&linux, job.id, new_upload("a.bin", &bytes))
            .expect("create");
        let cut = s
            .put_chunk(&linux, session.id, range(0, 99, 150), &mut &bytes[..60])
            .err()
            .expect("cut");
        assert_eq!((cut.code, cut.received), (Code::BadRequest, Some(60)));
        assert_eq!(s.upload(&linux, session.id).expect("get").received, 60);
        s.put_chunk(&linux, session.id, range(60, 149, 150), &mut &bytes[60..])
            .expect("resume");
        s.complete_upload(&linux, session.id).expect("complete");
    }

    #[test]
    fn a_bad_sha256_ends_the_session() {
        let (_d, s) = small();
        let linux = agent("linux");
        let job = s.create_job(&linux, task("windows")).expect("job");
        let mut upload = new_upload("a.bin", b"hello");
        upload.sha256 = sha(b"other");
        let (session, _) = s.create_upload(&linux, job.id, upload).expect("create");
        s.put_chunk(&linux, session.id, range(0, 4, 5), &mut &b"hello"[..])
            .expect("chunk");
        let err = s.complete_upload(&linux, session.id).err().expect("mismatch");
        assert_eq!(err.code, Code::ChecksumMismatch);
        assert_eq!(s.upload(&linux, session.id).err().map(|e| e.code), Some(Code::NotFound));
        assert!(!s.part_path(session.id.get()).exists());
        assert!(s.get_job(&linux, job.id).expect("job").attachments.is_empty());
    }

    #[test]
    fn sessions_belong_to_their_uploader() {
        let (_d, s) = small();
        let (linux, windows) = (agent("linux"), agent("windows"));
        let job = s.create_job(&linux, task("windows")).expect("job");
        let (session, _) = s
            .create_upload(&linux, job.id, new_upload("a.bin", b"abc"))
            .expect("create");
        let code = |r: ApiResult<()>| r.err().map(|e| e.code);
        assert_eq!(code(s.upload(&windows, session.id).map(drop)), Some(Code::NotYours));
        assert_eq!(
            code(
                s.put_chunk(&windows, session.id, range(0, 2, 3), &mut &b"abc"[..])
                    .map(drop)
            ),
            Some(Code::NotYours)
        );
        assert_eq!(
            code(s.complete_upload(&windows, session.id).map(drop)),
            Some(Code::NotYours)
        );
        assert_eq!(code(s.abort_upload(&windows, session.id)), Some(Code::NotYours));
        assert!(s.uploads_of(&windows, job.id).expect("list").is_empty());
        assert_eq!(s.uploads_of(&linux, job.id).expect("list").len(), 1);
        // Only the sender attaches, so the recipient cannot start one either.
        let theirs = s.create_upload(&windows, job.id, new_upload("b.bin", b"x"));
        assert_eq!(theirs.err().map(|e| e.code), Some(Code::NotYours));
        s.abort_upload(&linux, session.id).expect("abort");
        assert!(!s.part_path(session.id.get()).exists());
        assert!(s.uploads_of(&linux, job.id).expect("list").is_empty());
    }

    #[test]
    fn limits_per_file_job_and_quota() {
        let (_d, s) = small();
        let linux = agent("linux");
        let job = s.create_job(&linux, task("windows")).expect("job");
        let too_big = s.create_upload(&linux, job.id, new_upload("a.bin", &[0; 1001]));
        assert_eq!(too_big.err().map(|e| e.code), Some(Code::TooLarge));
        s.create_upload(&linux, job.id, new_upload("a.bin", &[0; 1000]))
            .expect("first");
        // 1000 open on the job; 600 more pass the 1500 per job.
        let over_job = s.create_upload(&linux, job.id, new_upload("b.bin", &[0; 600]));
        assert_eq!(over_job.err().map(|e| e.code), Some(Code::TooLarge));
        s.create_upload(&linux, job.id, new_upload("b.bin", &[0; 500]))
            .expect("fits the job");
        assert_eq!(
            s.quota_used(&linux).expect("used"),
            1500,
            "open uploads reserve their size"
        );

        let second = s.create_job(&linux, task("windows")).expect("job");
        s.create_upload(&linux, second.id, new_upload("c.bin", &[0; 1000]))
            .expect("2500 of 2500");
        let third = s
            .create_job(&linux, delivery(Kind::Message, "windows", "hi"))
            .expect("job");
        let over = s
            .create_upload(&linux, third.id, new_upload("d.bin", &[0; 1]))
            .err()
            .expect("quota");
        assert_eq!(over.code, Code::OverQuota);
        assert!(over.message.contains("2500"), "{}", over.message);
        // The quota is the sender's: the recipient's own uploads are not touched by it.
        assert_eq!(s.quota_used(&agent("windows")).expect("used"), 0);
    }

    #[test]
    fn the_free_space_reserve() {
        let (_d, s) = store();
        let free = Arc::new(AtomicU64::new(10_000));
        let reader = Arc::clone(&free);
        let s = s
            .with_upload_rules(UploadRules {
                disk_free_min_bytes: 4000,
                ..small_rules()
            })
            .with_free_space(Arc::new(move |_| Some(reader.load(Ordering::Relaxed))));
        let linux = agent("linux");
        let job = s.create_job(&linux, task("windows")).expect("job");
        s.create_upload(&linux, job.id, new_upload("a.bin", &[0; 1000]))
            .expect("fits");
        free.store(4500, Ordering::Relaxed);
        let full = s
            .create_upload(&linux, job.id, new_upload("b.bin", &[0; 501]))
            .err()
            .expect("full");
        assert_eq!((full.code, full.code.status()), (Code::InsufficientStorage, 507));
        assert_eq!(s.check_disk(501).err().map(|e| e.code), Some(Code::InsufficientStorage));
        s.check_disk(500).expect("exactly the reserve left");
        let unknown = s.with_free_space(Arc::new(|_| None));
        unknown
            .create_upload(&linux, job.id, new_upload("b.bin", &[0; 400]))
            .expect("unknown free space lets it through");
    }

    #[test]
    fn expiry_and_orphans_are_cleaned_and_the_quota_counts_live_data() {
        let (_d, s) = small();
        let linux = agent("linux");
        let job = s.create_job(&linux, task("windows")).expect("job");
        let (old, _) = s
            .create_upload(&linux, job.id, new_upload("a.bin", &[1; 300]))
            .expect("create");
        let (fresh, _) = s
            .create_upload(&linux, job.id, new_upload("b.bin", &[1; 200]))
            .expect("create");
        s.db()
            .execute("UPDATE uploads SET updated_at = 0 WHERE id = ?1", [old.id.get()])
            .expect("age it");
        assert_eq!(s.quota_used(&linux).expect("used"), 500);
        let purged = s.purge(clock::now()).expect("purge");
        assert_eq!(purged.uploads, [old.id.get()]);
        for id in &purged.uploads {
            s.remove_part(*id);
        }
        assert!(!s.part_path(old.id.get()).exists());
        assert!(s.part_path(fresh.id.get()).exists());
        assert_eq!(
            s.quota_used(&linux).expect("used"),
            200,
            "an expired session frees its reserve"
        );

        let stray = s.tmp.join("4242-7.part");
        let lost = s.part_path(9999);
        let gone_job = s.files.join("j9999");
        fs::write(&stray, b"x").expect("stray");
        fs::write(&lost, b"x").expect("lost");
        fs::create_dir_all(&gone_job).expect("dir");
        s.sweep_orphans(Duration::from_secs(3600));
        assert!(
            stray.exists() && lost.exists() && gone_job.exists(),
            "recent files are left alone"
        );
        s.sweep_orphans(Duration::ZERO);
        assert!(!stray.exists() && !lost.exists() && !gone_job.exists());
        assert!(s.part_path(fresh.id.get()).exists(), "a live session keeps its file");

        // A cancelled job takes its sessions and their files with it.
        s.cancel_job(&linux, job.id).expect("cancel");
        assert!(!s.part_path(fresh.id.get()).exists());
        assert_eq!(s.quota_used(&linux).expect("used"), 0);
    }

    #[test]
    fn purged_jobs_leave_the_quota() {
        let (_d, s) = small();
        let (linux, windows) = (agent("linux"), agent("windows"));
        let job = s
            .create_job(&linux, delivery(Kind::Message, "windows", "file"))
            .expect("job");
        let (session, _) = s
            .create_upload(&linux, job.id, new_upload("a.bin", &[5; 80]))
            .expect("create");
        fill(&s, &linux, session.id, &[5; 80], 30);
        s.complete_upload(&linux, session.id).expect("complete");
        assert_eq!(s.quota_used(&linux).expect("used"), 80);
        s.inbox(&windows, crate::types::Seq::default(), None, 50)
            .expect("deliver");
        s.db().execute("UPDATE jobs SET updated_at = 0", []).expect("age");
        let purged = s.purge(clock::now()).expect("purge");
        assert_eq!(purged.jobs, [job.id]);
        assert_eq!(s.quota_used(&linux).expect("used"), 0);
    }

    #[test]
    fn df_output() {
        let posix = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                     /dev/sda1 1000000 400000 600000 40% /var/lib/racking\n";
        assert_eq!(parse_df(posix), Some(600_000 * 1024));
        let spaced = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                      my disk 10 5 5 50% /mnt/my disk\n";
        assert_eq!(parse_df(spaced), Some(5 * 1024));
        assert_eq!(parse_df("Filesystem\n"), None);
        assert_eq!(parse_df(""), None);
    }

    #[test]
    fn old_databases_gain_the_uploads_table() {
        let dir = TempDir::new();
        fs::create_dir_all(&dir.0).expect("dir");
        {
            let conn = Connection::open(dir.0.join("racking.db")).expect("db");
            conn.execute_batch(
                "CREATE TABLE jobs (id INTEGER PRIMARY KEY AUTOINCREMENT, from_agent TEXT NOT NULL, \
                 to_agent TEXT NOT NULL, kind TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL, \
                 tags TEXT NOT NULL, origin TEXT, reply_to INTEGER, outcome TEXT, reason TEXT, \
                 state TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, \
                 seq INTEGER NOT NULL, updated_seq INTEGER NOT NULL) STRICT;
                 CREATE TABLE attachments (job INTEGER NOT NULL REFERENCES jobs (id) ON DELETE CASCADE, \
                 name TEXT NOT NULL, size INTEGER NOT NULL, sha256 TEXT NOT NULL, content_type TEXT, \
                 PRIMARY KEY (job, name)) STRICT;",
            )
            .expect("old schema");
        }
        let s = Store::open(&dir.0).expect("open").with_upload_rules(small_rules());
        let linux = agent("linux");
        let job = s.create_job(&linux, task("windows")).expect("job");
        s.create_upload(&linux, job.id, new_upload("a.bin", b"x"))
            .expect("uploads work");
    }
}
