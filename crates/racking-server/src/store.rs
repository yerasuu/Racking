//! Jobs, attachments, links and idempotency records in SQLite, plus the
//! attachment files under the data directory. Job content is stored as given
//! and never interpreted.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::clock;
use crate::error::{ApiError, ApiResult, Code};
use crate::presence;
use crate::types::{AgentName, AttachmentName, JobId, Seq, Sha256Hex, hex, truncate};

pub mod limits {
    pub const TITLE_CHARS: usize = 200;
    pub const BODY_BYTES: usize = 64 * 1024;
    /// A notice is a heads-up, so it stays short enough to read in one glance.
    pub const NOTICE_BYTES: usize = 1024;
    /// A report is a document to read later, so it may be long; it never comes inline.
    pub const REPORT_BYTES: usize = 1024 * 1024;
    /// Notices and messages up to this size come whole in `/inbox`; longer ones as a preview.
    pub const INLINE_BYTES: usize = 4 * 1024;
    /// Title the server derives from the first line of a message or notice.
    pub const DERIVED_TITLE_CHARS: usize = 80;
    pub const PREVIEW_CHARS: usize = 200;
    pub const TAGS: usize = 16;
    pub const TAG_CHARS: usize = 40;
    pub const FILE_BYTES: u64 = 64 * 1024 * 1024;
    pub const JOB_FILE_BYTES: u64 = 256 * 1024 * 1024;
    pub const FILES_PER_JOB: usize = 16;
    /// One published `rack` binary; a release build is a few MiB.
    pub const DIST_BYTES: u64 = 32 * 1024 * 1024;
    pub const WAIT_DEFAULT_SECS: u64 = 25;
    pub const WAIT_MAX_SECS: u64 = 55;
    pub const IDEMPOTENCY_KEY_CHARS: usize = 80;
    pub const LIST_DEFAULT: u32 = 50;
    pub const LIST_MAX: u32 = 200;
    pub const REASON_CHARS: usize = 2000;
    pub const LINK_FIELD_CHARS: usize = 200;
    /// Idempotency records live this long, per the protocol.
    pub const IDEMPOTENCY_TTL_SECS: i64 = 24 * 3600;
    /// Closed jobs (done, rejected, failed) are kept this long, per the protocol.
    pub const CLOSED_JOB_TTL_SECS: i64 = 30 * 24 * 3600;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Task,
    Comment,
    Result,
    Message,
    Notice,
    Report,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Task => "task",
            Kind::Comment => "comment",
            Kind::Result => "result",
            Kind::Message => "message",
            Kind::Notice => "notice",
            Kind::Report => "report",
        }
    }

    /// Messages, notices and reports are delivered, not worked on: they have no
    /// accept, reject or result, and the inbox read that hands them over receives them.
    pub fn is_delivery(self) -> bool {
        matches!(self, Kind::Message | Kind::Notice | Kind::Report)
    }

    /// Whether `/inbox` carries the whole body instead of a preview.
    fn inlines(self, body_size: i64) -> bool {
        matches!(self, Kind::Message | Kind::Notice) && body_size <= limits::INLINE_BYTES as i64
    }
}

impl FromStr for Kind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "task" => Ok(Kind::Task),
            "comment" => Ok(Kind::Comment),
            "result" => Ok(Kind::Result),
            "message" => Ok(Kind::Message),
            "notice" => Ok(Kind::Notice),
            "report" => Ok(Kind::Report),
            other => Err(format!(
                "kind `{other}` must be task, message, notice, report, comment or result"
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Sent,
    Received,
    Accepted,
    Rejected,
    Done,
    Failed,
    /// A message or report that expected a reply got one from its recipient.
    Answered,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Sent => "sent",
            State::Received => "received",
            State::Accepted => "accepted",
            State::Rejected => "rejected",
            State::Done => "done",
            State::Failed => "failed",
            State::Answered => "answered",
        }
    }
}

impl FromStr for State {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "sent" => Ok(State::Sent),
            "received" => Ok(State::Received),
            "accepted" => Ok(State::Accepted),
            "rejected" => Ok(State::Rejected),
            "done" => Ok(State::Done),
            "failed" => Ok(State::Failed),
            "answered" => Ok(State::Answered),
            other => Err(format!(
                "state `{other}` must be sent, received, accepted, rejected, done, failed or answered"
            )),
        }
    }
}

/// Shown to the recipient; the service never orders or filters by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    Low,
    Normal,
    High,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Low => "low",
            Priority::Normal => "normal",
            Priority::High => "high",
        }
    }
}

impl FromStr for Priority {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "low" => Ok(Priority::Low),
            "normal" => Ok(Priority::Normal),
            "high" => Ok(Priority::High),
            other => Err(format!("priority `{other}` must be low, normal or high")),
        }
    }
}

/// How a `result` job closes the job it answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Failed,
}

impl Outcome {
    fn state(self) -> State {
        match self {
            Outcome::Done => State::Done,
            Outcome::Failed => State::Failed,
        }
    }
}

impl FromStr for Outcome {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "done" => Ok(Outcome::Done),
            "failed" => Ok(Outcome::Failed),
            other => Err(format!("outcome `{other}` must be done or failed")),
        }
    }
}

/// A request to move a job along the state graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    Receive,
    Accept,
    Reject,
    Finish(Outcome),
}

impl Move {
    fn verb(self) -> &'static str {
        match self {
            Move::Receive => "receive",
            Move::Accept => "accept",
            Move::Reject => "reject",
            Move::Finish(Outcome::Done) => "finish as done",
            Move::Finish(Outcome::Failed) => "finish as failed",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    To(State),
    /// The job is already where the move leads; retries land here.
    Repeat,
    Refused,
}

/// The state graph of the protocol: `sent -> received -> accepted -> done|failed`,
/// and `sent|received -> rejected`.
pub fn step(current: State, mv: Move) -> Step {
    match (mv, current) {
        (Move::Receive, State::Sent) => Step::To(State::Received),
        (Move::Receive, State::Received) => Step::Repeat,
        (Move::Accept, State::Received) => Step::To(State::Accepted),
        (Move::Accept, State::Accepted) => Step::Repeat,
        (Move::Reject, State::Sent | State::Received) => Step::To(State::Rejected),
        (Move::Reject, State::Rejected) => Step::Repeat,
        (Move::Finish(outcome), State::Accepted) => Step::To(outcome.state()),
        (Move::Finish(outcome), s) if s == outcome.state() => Step::Repeat,
        // Every other pair is outside the graph; listing them would only repeat `Refused`.
        _ => Step::Refused,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    pub repo: String,
    pub id: String,
    #[serde(default)]
    pub url: Option<String>,
}

/// A job checked at the HTTP edge, ready to store.
pub struct NewJob {
    pub to: AgentName,
    pub kind: Kind,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub origin: Option<Origin>,
    pub reply_to: Option<JobId>,
    pub outcome: Option<Outcome>,
    pub priority: Priority,
    pub expects_reply: bool,
    /// The model the sender suggests, already lowercased and checked; a hint, never an order.
    pub model: Option<String>,
}

pub struct Attachment {
    pub name: String,
    pub size: i64,
    pub sha256: String,
}

impl Attachment {
    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "size": self.size, "sha256": self.sha256})
    }
}

/// Whether an inbox read hands over the notices, messages and reports it lists.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Deliver,
    /// Lists without moving anything to `received`, so a client can count what is
    /// waiting (a stop guard) without taking the jobs from the agent that reads them.
    Peek,
}

/// Whether a job is shown whole or as an inbox summary.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Full,
    Summary,
}

pub struct Job {
    pub id: JobId,
    pub from: String,
    pub to: String,
    pub kind: Kind,
    pub title: String,
    /// The whole body for `View::Full`; for `View::Summary`, the whole body of a
    /// message or notice that `Kind::inlines`, else the first 200 chars.
    pub body: String,
    pub body_size: i64,
    pub tags: Vec<String>,
    pub origin: Option<Origin>,
    pub reply_to: Option<JobId>,
    pub outcome: Option<String>,
    pub reason: Option<String>,
    pub state: State,
    pub created_at: i64,
    pub updated_at: i64,
    pub seq: i64,
    pub updated_seq: i64,
    pub priority: Priority,
    pub expects_reply: bool,
    pub model: Option<String>,
    pub attachments: Vec<Attachment>,
}

impl Job {
    pub fn to_json(&self, view: View) -> Value {
        let mut v = json!({
            "id": self.id.to_string(),
            "from": self.from,
            "to": self.to,
            "kind": self.kind.as_str(),
            "title": self.title,
            "tags": self.tags,
            "origin": self.origin,
            "reply_to": self.reply_to.map(|j| j.to_string()),
            "state": self.state.as_str(),
            "attachments": self.attachments.iter().map(Attachment::to_json).collect::<Vec<_>>(),
            "created_at": clock::rfc3339(self.created_at),
            "updated_at": clock::rfc3339(self.updated_at),
            "seq": self.seq,
            "updated_seq": self.updated_seq,
            "body_size": self.body_size,
            "priority": self.priority.as_str(),
            "expects_reply": self.expects_reply,
            "model": self.model,
        });
        if let Some(obj) = v.as_object_mut() {
            let mut insert = |k: &str, value: Value| obj.insert(k.to_owned(), value);
            match view {
                View::Full => {
                    insert("body", Value::from(self.body.as_str()));
                    insert("body_truncated", Value::from(false));
                }
                View::Summary if self.kind.inlines(self.body_size) => {
                    insert("body", Value::from(self.body.as_str()));
                    insert("body_preview", Value::from(truncate(&self.body, limits::PREVIEW_CHARS)));
                    insert("body_truncated", Value::from(false));
                }
                View::Summary => {
                    insert("body_preview", Value::from(self.body.as_str()));
                    insert("body_truncated", Value::from(self.body_size > self.body.len() as i64));
                }
            }
            if let Some(outcome) = &self.outcome {
                obj.insert("outcome".to_owned(), Value::from(outcome.as_str()));
            }
            if let Some(reason) = &self.reason {
                obj.insert("reason".to_owned(), Value::from(reason.as_str()));
            }
        }
        v
    }

    fn is_party(&self, me: &AgentName) -> bool {
        self.from == me.as_str() || self.to == me.as_str()
    }
}

/// A page of jobs and the cursor to pass as `since` next time.
pub struct Listing {
    pub jobs: Vec<Job>,
    pub next: i64,
}

impl Listing {
    pub fn to_json(&self) -> Value {
        json!({
            "jobs": self.jobs.iter().map(|j| j.to_json(View::Summary)).collect::<Vec<_>>(),
            "next": self.next,
        })
    }
}

pub struct Link {
    pub job: JobId,
    pub repo: String,
    pub local_id: String,
    pub url: Option<String>,
    pub set_by: String,
    pub updated_at: i64,
}

impl Link {
    pub fn to_json(&self) -> Value {
        json!({
            "job": self.job.to_string(),
            "repo": self.repo,
            "id": self.local_id,
            "url": self.url,
            "set_by": self.set_by,
            "updated_at": clock::rfc3339(self.updated_at),
        })
    }
}

/// Outcome of looking up an `Idempotency-Key` before running a write.
#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    /// First use: run the request, then `finish_key` or `release_key`.
    Fresh,
    Replay {
        status: u16,
        body: String,
    },
    /// The same key is being served by another request right now.
    InFlight,
    Conflict,
}

/// An open attachment ready to stream back.
pub struct Download {
    pub file: File,
    pub sha256: String,
}

/// Wakes `wait` requests when any job changes. The version is guarded by a
/// mutex only because `Condvar` needs one; it is not a general lock.
struct Changes {
    version: Mutex<u64>,
    cv: Condvar,
}

impl Changes {
    fn current(&self) -> u64 {
        *self.version.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn bump(&self) {
        let mut v = self.version.lock().unwrap_or_else(PoisonError::into_inner);
        *v = v.wrapping_add(1);
        self.cv.notify_all();
    }

    /// Sleeps until the version moves past `seen` or `deadline` passes.
    fn wait_past(&self, seen: u64, deadline: Instant) {
        let mut v = self.version.lock().unwrap_or_else(PoisonError::into_inner);
        while *v == seen {
            let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else {
                return;
            };
            v = self.cv.wait_timeout(v, left).unwrap_or_else(PoisonError::into_inner).0;
        }
    }
}

const JOB_COLUMNS: &str = "id, from_agent, to_agent, kind, title, body, length(CAST(body AS BLOB)), tags, origin, \
                           reply_to, outcome, reason, state, created_at, updated_at, seq, updated_seq, priority, \
                           expects_reply, model";
/// The body column must agree with `Kind::inlines`, `limits::INLINE_BYTES` and `limits::PREVIEW_CHARS`.
const SUMMARY_COLUMNS: &str = "id, from_agent, to_agent, kind, title, \
                               CASE WHEN kind IN ('message', 'notice') AND length(CAST(body AS BLOB)) <= 4096 \
                               THEN body ELSE substr(body, 1, 200) END, length(CAST(body AS BLOB)), \
                               tags, origin, reply_to, outcome, reason, state, created_at, updated_at, seq, updated_seq, \
                               priority, expects_reply, model";

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_agent TEXT NOT NULL,
    to_agent TEXT NOT NULL,
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    tags TEXT NOT NULL,
    origin TEXT,
    reply_to INTEGER,
    outcome TEXT,
    reason TEXT,
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL,
    priority TEXT NOT NULL DEFAULT 'normal',
    expects_reply INTEGER NOT NULL DEFAULT 0,
    model TEXT
) STRICT;
CREATE INDEX IF NOT EXISTS jobs_inbox ON jobs (to_agent, seq);
CREATE INDEX IF NOT EXISTS jobs_outbox ON jobs (from_agent, updated_seq);
CREATE TABLE IF NOT EXISTS attachments (
    job INTEGER NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    size INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    content_type TEXT,
    PRIMARY KEY (job, name)
) STRICT;
CREATE TABLE IF NOT EXISTS links (
    job INTEGER PRIMARY KEY REFERENCES jobs (id) ON DELETE CASCADE,
    repo TEXT NOT NULL,
    local_id TEXT NOT NULL,
    url TEXT,
    set_by TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS idempotency (
    agent TEXT NOT NULL,
    key TEXT NOT NULL,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    body_hash TEXT NOT NULL,
    status INTEGER,
    response TEXT,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (agent, key)
) STRICT;
CREATE TABLE IF NOT EXISTS agents (
    name TEXT PRIMARY KEY,
    last_seen INTEGER,
    about TEXT,
    rack_version TEXT,
    os TEXT,
    capabilities TEXT
) STRICT;
";

pub struct Store {
    /// One connection for all requests: `rusqlite::Connection` is not `Sync`,
    /// and two agents never make the lock a bottleneck.
    conn: Mutex<Connection>,
    files: PathBuf,
    tmp: PathBuf,
    changes: Changes,
    tmp_counter: AtomicU64,
    last_purge: AtomicI64,
}

impl Store {
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        let files = data_dir.join("attachments");
        let tmp = data_dir.join("tmp");
        for dir in [data_dir, &files, &tmp] {
            fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let db_path = data_dir.join("racking.db");
        let conn = Connection::open(&db_path).map_err(|e| format!("cannot open {}: {e}", db_path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| format!("cannot set up the schema: {e}"))?;
        add_reply_columns(&conn).map_err(|e| format!("cannot add the priority columns: {e}"))?;
        add_hint_columns(&conn).map_err(|e| format!("cannot add the model and capabilities columns: {e}"))?;
        Ok(Self {
            conn: Mutex::new(conn),
            files,
            tmp,
            changes: Changes {
                version: Mutex::new(0),
                cv: Condvar::new(),
            },
            tmp_counter: AtomicU64::new(0),
            last_purge: AtomicI64::new(0),
        })
    }

    /// A panic while holding the lock cannot leave a transaction open (it rolls
    /// back on drop), so a poisoned connection is still consistent.
    fn db(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn create_job(&self, me: &AgentName, new: NewJob) -> ApiResult<Job> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        let now = clock::now();
        if let Some(reply_id) = new.reply_to {
            let original = load_job(&tx, reply_id, View::Summary)?;
            if !original.is_party(me) {
                return Err(not_yours(&original));
            }
            let other = if original.from == me.as_str() {
                &original.to
            } else {
                &original.from
            };
            if new.to.as_str() != other {
                return Err(ApiError::bad_request(format!(
                    "to: a reply to {reply_id} goes to {other}, not {}",
                    new.to
                )));
            }
            match (original.kind, new.kind) {
                (Kind::Notice, _) => {
                    return Err(ApiError::new(
                        Code::BadTransition,
                        format!("job {reply_id} is a notice, which takes no reply"),
                        "Send a message instead: kind \"message\" without reply_to.",
                    ));
                }
                (Kind::Message | Kind::Report, Kind::Comment | Kind::Result) => {
                    return Err(ApiError::new(
                        Code::BadTransition,
                        format!(
                            "job {reply_id} is a {}, which takes no comment or result",
                            original.kind.as_str()
                        ),
                        format!(
                            "It needs no reply protocol; answer with a message: kind \"message\", reply_to {reply_id}."
                        ),
                    ));
                }
                _ => {}
            }
            if original.kind.is_delivery()
                && original.expects_reply
                && original.to == me.as_str()
                && matches!(original.state, State::Sent | State::Received)
            {
                set_state(&tx, original.id, State::Answered, None, now)?;
            }
            if new.kind == Kind::Result {
                if original.to != me.as_str() {
                    return Err(ApiError::new(
                        Code::NotYours,
                        format!("only {} can report the result of {reply_id}", original.to),
                        "Send a comment instead: kind \"comment\".",
                    ));
                }
                let outcome = new.outcome.unwrap_or(Outcome::Done);
                apply_move(&tx, &original, Move::Finish(outcome), None, now)?;
            }
        }
        let seq = bump_counter(&tx, &format!("inbox:{}", new.to))?;
        let updated_seq = bump_counter(&tx, "updated")?;
        let tags = serde_json::to_string(&new.tags).map_err(|e| ApiError::internal("encode tags", &e))?;
        let origin = new
            .origin
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| ApiError::internal("encode origin", &e))?;
        let outcome = (new.kind == Kind::Result).then(|| new.outcome.unwrap_or(Outcome::Done).state().as_str());
        tx.execute(
            "INSERT INTO jobs (from_agent, to_agent, kind, title, body, tags, origin, reply_to, outcome, state, \
             created_at, updated_at, seq, updated_seq, priority, expects_reply, model) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'sent', ?10, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                me.as_str(),
                new.to.as_str(),
                new.kind.as_str(),
                new.title,
                new.body,
                tags,
                origin,
                new.reply_to.map(JobId::get),
                outcome,
                now,
                seq,
                updated_seq,
                new.priority.as_str(),
                new.expects_reply,
                new.model
            ],
        )?;
        let id = JobId::new(tx.last_insert_rowid());
        let job = load_job(&tx, id, View::Full)?;
        tx.commit()?;
        drop(conn);
        self.changes.bump();
        Ok(job)
    }

    /// Reading a job as its recipient marks it `received`, as the protocol's
    /// "implicit on first full read" asks; for a message, notice or report that is delivery.
    pub fn get_job(&self, me: &AgentName, id: JobId) -> ApiResult<Job> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        let job = load_job(&tx, id, View::Full)?;
        if !job.is_party(me) {
            return Err(not_yours(&job));
        }
        if job.to != me.as_str() || job.state != State::Sent {
            return Ok(job);
        }
        apply_move(&tx, &job, Move::Receive, None, clock::now())?;
        let job = load_job(&tx, id, View::Full)?;
        tx.commit()?;
        drop(conn);
        self.changes.bump();
        Ok(job)
    }

    /// Applies a recipient move. `reason` is stored for `Move::Reject`.
    pub fn move_job(&self, me: &AgentName, id: JobId, mv: Move, reason: Option<&str>) -> ApiResult<Job> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        let job = load_job(&tx, id, View::Full)?;
        if !job.is_party(me) {
            return Err(not_yours(&job));
        }
        if job.to != me.as_str() {
            return Err(ApiError::new(
                Code::NotYours,
                format!("only the recipient {} can {} {id}", job.to, mv.verb()),
                format!("As the sender you can cancel while it is sent: POST /jobs/{id}/cancel."),
            ));
        }
        if job.kind.is_delivery() {
            let hint = match job.kind {
                Kind::Notice => "A notice needs nothing back; reading your inbox already received it.".to_owned(),
                _ => format!(
                    "A {} needs no reply protocol; answer with a message (kind \"message\", reply_to {id}) if you want.",
                    job.kind.as_str()
                ),
            };
            return Err(ApiError::new(
                Code::BadTransition,
                format!("job {id} is a {}, it cannot {}", job.kind.as_str(), mv.verb()),
                hint,
            ));
        }
        let changed = apply_move(&tx, &job, mv, reason, clock::now())?;
        let job = load_job(&tx, id, View::Full)?;
        tx.commit()?;
        drop(conn);
        if changed {
            self.changes.bump();
        }
        Ok(job)
    }

    /// Deletes a job the caller sent while it is still `sent`.
    pub fn cancel_job(&self, me: &AgentName, id: JobId) -> ApiResult<()> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        let job = load_job(&tx, id, View::Summary)?;
        if !job.is_party(me) {
            return Err(not_yours(&job));
        }
        if job.from != me.as_str() {
            return Err(ApiError::new(
                Code::NotYours,
                format!("only the sender {} can cancel {id}", job.from),
                format!("As the recipient, reject it: POST /jobs/{id}/reject with a reason."),
            ));
        }
        if job.state != State::Sent {
            return Err(ApiError::new(
                Code::BadTransition,
                format!("job {id} is {}, cannot cancel", job.state.as_str()),
                "Only a job still `sent` can be cancelled; send a comment to the recipient instead.",
            ));
        }
        tx.execute("DELETE FROM jobs WHERE id = ?1", [id.get()])?;
        tx.commit()?;
        drop(conn);
        self.remove_job_files(id);
        self.changes.bump();
        Ok(())
    }

    fn job_dir(&self, id: JobId) -> PathBuf {
        self.files.join(id.to_string())
    }

    fn remove_job_files(&self, id: JobId) {
        let dir = self.job_dir(id);
        if let Err(e) = fs::remove_dir_all(&dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(target: "racking::store", job = %id, error = %e, "could not remove attachment files");
        }
    }

    /// Streams an upload to a temporary file, checks size and `claimed` hash,
    /// then moves it into place if the job is still `sent`.
    pub fn put_attachment(
        &self,
        me: &AgentName,
        id: JobId,
        name: &AttachmentName,
        body: &mut dyn Read,
        claimed: &Sha256Hex,
        content_type: Option<&str>,
    ) -> ApiResult<Attachment> {
        let budget = {
            let conn = self.db();
            let job = load_job(&conn, id, View::Summary)?;
            check_upload(&job, me, name)?
        };
        let tmp_path = self.tmp.join(format!(
            "{}-{}.part",
            std::process::id(),
            self.tmp_counter.fetch_add(1, Ordering::Relaxed)
        ));
        let too_large = || too_large_for_job(budget);
        let result = receive_file(&tmp_path, body, budget.max_bytes, too_large, claimed).and_then(|size| {
            let mut conn = self.db();
            let tx = conn.transaction()?;
            let job = load_job(&tx, id, View::Summary)?;
            let budget = check_upload(&job, me, name)?;
            if size > budget.max_bytes {
                return Err(too_large_for_job(budget));
            }
            let dir = self.job_dir(id);
            fs::create_dir_all(&dir).map_err(|e| ApiError::internal("create the attachment directory", &e))?;
            fs::rename(&tmp_path, dir.join(name.as_str()))
                .map_err(|e| ApiError::internal("store the attachment", &e))?;
            let size = i64::try_from(size).map_err(|e| ApiError::internal("record the attachment size", &e))?;
            tx.execute(
                "INSERT INTO attachments (job, name, size, sha256, content_type) VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT (job, name) DO UPDATE SET size = ?3, sha256 = ?4, content_type = ?5",
                params![id.get(), name.as_str(), size, claimed.as_str(), content_type],
            )?;
            tx.commit()?;
            Ok(Attachment {
                name: name.as_str().to_owned(),
                size,
                sha256: claimed.as_str().to_owned(),
            })
        });
        if result.is_err()
            && let Err(e) = fs::remove_file(&tmp_path)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(target: "racking::store", path = %tmp_path.display(), error = %e, "could not remove temp file");
        }
        result
    }

    /// Opens an attachment while holding the lock, so a concurrent cancel cannot
    /// delete it between the check and the open.
    pub fn get_attachment(&self, me: &AgentName, id: JobId, name: &AttachmentName) -> ApiResult<Download> {
        let conn = self.db();
        let job = load_job(&conn, id, View::Summary)?;
        if !job.is_party(me) {
            return Err(not_yours(&job));
        }
        let Some(att) = job.attachments.into_iter().find(|a| a.name == name.as_str()) else {
            return Err(ApiError::new(
                Code::NotFound,
                format!("job {id} has no attachment {}", name.as_str()),
                format!("GET /jobs/{id} lists its attachments."),
            ));
        };
        let file = File::open(self.job_dir(id).join(name.as_str()))
            .map_err(|e| ApiError::internal("open the attachment", &e))?;
        Ok(Download {
            file,
            sha256: att.sha256,
        })
    }

    /// Lists the caller's inbox. The answer hands over the messages, notices and
    /// reports in it, so those still `sent` become `received` here, where their
    /// sender's outbox sees it.
    pub fn inbox(&self, me: &AgentName, since: Seq, state: Option<State>, limit: u32) -> ApiResult<Listing> {
        self.inbox_with(me, since, state, limit, Delivery::Deliver)
    }

    /// `inbox`, or with `Delivery::Peek` the same listing with nothing handed over.
    pub fn inbox_with(
        &self,
        me: &AgentName,
        since: Seq,
        state: Option<State>,
        limit: u32,
        delivery: Delivery,
    ) -> ApiResult<Listing> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        let mut jobs = {
            let mut stmt = tx.prepare_cached(&format!(
                "SELECT {SUMMARY_COLUMNS} FROM jobs WHERE to_agent = ?1 AND seq > ?2 \
                 AND (?3 IS NULL OR state = ?3) ORDER BY seq LIMIT ?4"
            ))?;
            let rows = stmt.query_map(
                params![me.as_str(), since.get(), state.map(State::as_str), limit],
                job_from_row,
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let now = clock::now();
        let mut delivered = false;
        for job in &mut jobs {
            job.attachments = load_attachments(&tx, job.id)?;
            if delivery == Delivery::Deliver && job.kind.is_delivery() && job.state == State::Sent {
                job.updated_seq = set_state(&tx, job.id, State::Received, None, now)?;
                job.state = State::Received;
                job.updated_at = now;
                delivered = true;
            }
        }
        tx.commit()?;
        drop(conn);
        if delivered {
            self.changes.bump();
        }
        let next = jobs.last().map_or(since.get(), |j| j.seq);
        Ok(Listing { jobs, next })
    }

    pub fn outbox(&self, me: &AgentName, since: Seq, limit: u32) -> ApiResult<Listing> {
        let conn = self.db();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM jobs WHERE from_agent = ?1 AND updated_seq > ?2 \
             ORDER BY updated_seq LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![me.as_str(), since.get(), limit], job_from_row)?;
        let mut jobs = Vec::new();
        for row in rows {
            let mut job = row?;
            job.attachments = load_attachments(&conn, job.id)?;
            jobs.push(job);
        }
        let next = jobs.last().map_or(since.get(), |j| j.updated_seq);
        Ok(Listing { jobs, next })
    }

    /// Runs `list` until it returns jobs or `deadline` passes. The change version
    /// is read before each query so a change between query and sleep is not missed.
    pub fn wait<F>(&self, deadline: Instant, mut list: F) -> ApiResult<Listing>
    where
        F: FnMut(&Self) -> ApiResult<Listing>,
    {
        loop {
            let seen = self.changes.current();
            let listing = list(self)?;
            if !listing.jobs.is_empty() || Instant::now() >= deadline {
                return Ok(listing);
            }
            self.changes.wait_past(seen, deadline);
        }
    }

    /// Records the receiver's local entry for a job it received.
    pub fn put_link(
        &self,
        me: &AgentName,
        id: JobId,
        repo: &str,
        local_id: &str,
        url: Option<&str>,
    ) -> ApiResult<Link> {
        let conn = self.db();
        let job = load_job(&conn, id, View::Summary)?;
        if !job.is_party(me) {
            return Err(not_yours(&job));
        }
        if job.to != me.as_str() {
            return Err(ApiError::new(
                Code::NotYours,
                format!("only the recipient {} records the link of {id}", job.to),
                "Read it instead: GET /links?job=ID.",
            ));
        }
        let now = clock::now();
        conn.execute(
            "INSERT INTO links (job, repo, local_id, url, set_by, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (job) DO UPDATE SET repo = ?2, local_id = ?3, url = ?4, set_by = ?5, updated_at = ?6",
            params![id.get(), repo, local_id, url, me.as_str(), now],
        )?;
        Ok(Link {
            job: id,
            repo: repo.to_owned(),
            local_id: local_id.to_owned(),
            url: url.map(str::to_owned),
            set_by: me.as_str().to_owned(),
            updated_at: now,
        })
    }

    /// Links of the caller's jobs (sent or received), or of one job.
    pub fn links(&self, me: &AgentName, job: Option<JobId>) -> ApiResult<Vec<Link>> {
        let conn = self.db();
        if let Some(id) = job {
            let j = load_job(&conn, id, View::Summary)?;
            if !j.is_party(me) {
                return Err(not_yours(&j));
            }
        }
        let mut stmt = conn.prepare_cached(
            "SELECT l.job, l.repo, l.local_id, l.url, l.set_by, l.updated_at FROM links l JOIN jobs j ON j.id = l.job \
             WHERE (j.from_agent = ?1 OR j.to_agent = ?1) AND (?2 IS NULL OR l.job = ?2) ORDER BY l.job LIMIT 1000",
        )?;
        let rows = stmt.query_map(params![me.as_str(), job.map(JobId::get)], |row| {
            Ok(Link {
                job: JobId::new(row.get(0)?),
                repo: row.get(1)?,
                local_id: row.get(2)?,
                url: row.get(3)?,
                set_by: row.get(4)?,
                updated_at: row.get(5)?,
            })
        })?;
        rows.map(|r| r.map_err(ApiError::from)).collect()
    }

    /// Looks up `key` and, on first use, reserves it so a concurrent retry
    /// cannot run the same write twice.
    pub fn claim_key(&self, me: &AgentName, key: &str, method: &str, path: &str, body_hash: &str) -> ApiResult<Claim> {
        let conn = self.db();
        let now = clock::now();
        let existing = conn
            .query_row(
                "SELECT method, path, body_hash, status, response, created_at FROM idempotency \
                 WHERE agent = ?1 AND key = ?2",
                params![me.as_str(), key],
                |row| {
                    Ok(StoredKey {
                        method: row.get(0)?,
                        path: row.get(1)?,
                        body_hash: row.get(2)?,
                        status: row.get(3)?,
                        response: row.get(4)?,
                        created_at: row.get(5)?,
                    })
                },
            )
            .optional()?;
        if let Some(stored) = existing.filter(|s| s.created_at > now - limits::IDEMPOTENCY_TTL_SECS) {
            if stored.method != method || stored.path != path || stored.body_hash != body_hash {
                return Ok(Claim::Conflict);
            }
            return Ok(match (stored.status, stored.response) {
                (Some(status), Some(body)) => Claim::Replay { status, body },
                (None, _) | (Some(_), None) => Claim::InFlight,
            });
        }
        conn.execute(
            "INSERT INTO idempotency (agent, key, method, path, body_hash, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (agent, key) DO UPDATE SET method = ?3, path = ?4, body_hash = ?5, status = NULL, \
             response = NULL, created_at = ?6",
            params![me.as_str(), key, method, path, body_hash, now],
        )?;
        Ok(Claim::Fresh)
    }

    pub fn finish_key(&self, me: &AgentName, key: &str, status: u16, body: &str) -> ApiResult<()> {
        self.db().execute(
            "UPDATE idempotency SET status = ?3, response = ?4 WHERE agent = ?1 AND key = ?2",
            params![me.as_str(), key, status, body],
        )?;
        Ok(())
    }

    /// Forgets a reserved key whose request failed on the server side, so a retry runs again.
    pub fn release_key(&self, me: &AgentName, key: &str) -> ApiResult<()> {
        self.db().execute(
            "DELETE FROM idempotency WHERE agent = ?1 AND key = ?2",
            params![me.as_str(), key],
        )?;
        Ok(())
    }

    /// Presence rows kept from earlier runs, for `Presence::new`.
    pub fn load_agents(&self) -> Result<Vec<presence::Stored>, String> {
        let conn = self.db();
        let mut stmt = conn
            .prepare("SELECT name, last_seen, about, rack_version, os, capabilities FROM agents")
            .map_err(|e| format!("cannot read the agents table: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(presence::Stored {
                    name: row.get(0)?,
                    last_seen: row.get(1)?,
                    info: presence::Info {
                        about: row.get(2)?,
                        rack_version: row.get(3)?,
                        os: row.get(4)?,
                        capabilities: row
                            .get::<_, Option<JsonCol<Vec<String>>>>(5)?
                            .map(|c| c.0)
                            .unwrap_or_default(),
                    },
                })
            })
            .map_err(|e| format!("cannot read the agents table: {e}"))?;
        rows.collect::<Result<_, _>>()
            .map_err(|e| format!("cannot read the agents table: {e}"))
    }

    /// Writes what `Presence::touch` found due: `last_seen` (never moved back) and the client software.
    pub fn save_seen(&self, agent: &AgentName, seen: &presence::Flush) -> ApiResult<()> {
        self.db().execute(
            "INSERT INTO agents (name, last_seen, rack_version, os) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (name) DO UPDATE SET last_seen = max(coalesce(last_seen, ?2), ?2), \
             rack_version = ?3, os = ?4",
            params![agent.as_str(), seen.at, seen.rack_version, seen.os],
        )?;
        Ok(())
    }

    pub fn save_agent_info(&self, agent: &AgentName, info: &presence::Info) -> ApiResult<()> {
        let capabilities = (!info.capabilities.is_empty())
            .then(|| serde_json::to_string(&info.capabilities))
            .transpose()
            .map_err(|e| ApiError::internal("encode capabilities", &e))?;
        self.db().execute(
            "INSERT INTO agents (name, about, rack_version, os, capabilities) VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (name) DO UPDATE SET about = ?2, rack_version = ?3, os = ?4, capabilities = ?5",
            params![agent.as_str(), info.about, info.rack_version, info.os, capabilities],
        )?;
        Ok(())
    }

    /// Drops expired idempotency records and closed jobs past their retention,
    /// at most once an hour; called from request handling so no extra thread lives.
    pub fn purge_if_due(&self) {
        let now = clock::now();
        let last = self.last_purge.load(Ordering::Relaxed);
        if now - last < 3600
            || self
                .last_purge
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        match self.purge(now) {
            Ok(ids) => {
                for id in &ids {
                    self.remove_job_files(*id);
                }
                if !ids.is_empty() {
                    tracing::info!(target: "racking::store", jobs = ids.len(), "purged closed jobs");
                }
            }
            Err(e) => tracing::warn!(target: "racking::store", error = %e.message, "purge failed"),
        }
    }

    fn purge(&self, now: i64) -> ApiResult<Vec<JobId>> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM idempotency WHERE created_at <= ?1",
            [now - limits::IDEMPOTENCY_TTL_SECS],
        )?;
        let ids = {
            // A delivered notice, message or report that asked for no reply is as closed as a done task.
            let mut stmt = tx.prepare(
                "DELETE FROM jobs WHERE updated_at <= ?1 AND (state IN ('done', 'rejected', 'failed', 'answered') \
                 OR (state = 'received' AND kind IN ('message', 'notice', 'report') AND expects_reply = 0)) \
                 RETURNING id",
            )?;
            let rows = stmt.query_map([now - limits::CLOSED_JOB_TTL_SECS], |row| row.get(0).map(JobId::new))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        tx.commit()?;
        Ok(ids)
    }
}

struct StoredKey {
    method: String,
    path: String,
    body_hash: String,
    status: Option<u16>,
    response: Option<String>,
    created_at: i64,
}

/// Streams `body` into a new file at `path`, refusing more than `max_bytes`
/// (with the caller's `too_large` error) and bytes that do not hash to `claimed`.
/// The caller removes `path` on error.
pub fn receive_file(
    path: &Path,
    body: &mut dyn Read,
    max_bytes: u64,
    too_large: impl FnOnce() -> ApiError,
    claimed: &Sha256Hex,
) -> ApiResult<u64> {
    let mut file = File::create(path).map_err(|e| ApiError::internal("create a temp file", &e))?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(ApiError::bad_request(format!("upload interrupted: {e}")));
            }
        };
        size += n as u64;
        if size > max_bytes {
            return Err(too_large());
        }
        let chunk = &buf[..n];
        hasher.update(chunk);
        file.write_all(chunk)
            .map_err(|e| ApiError::internal("write the upload", &e))?;
    }
    file.sync_all()
        .map_err(|e| ApiError::internal("flush the upload", &e))?;
    let actual = Sha256Hex::of_digest(&hasher.finalize());
    if &actual != claimed {
        return Err(ApiError::new(
            Code::ChecksumMismatch,
            format!(
                "X-Sha256 is {} but the uploaded bytes hash to {}",
                claimed.as_str(),
                actual.as_str()
            ),
            "Upload again; the file may have changed or the transfer was cut.",
        ));
    }
    Ok(size)
}

/// What an upload may still add to a job.
#[derive(Clone, Copy)]
struct Budget {
    max_bytes: u64,
}

fn check_upload(job: &Job, me: &AgentName, name: &AttachmentName) -> ApiResult<Budget> {
    if !job.is_party(me) {
        return Err(not_yours(job));
    }
    if job.from != me.as_str() {
        return Err(ApiError::new(
            Code::NotYours,
            format!("only the sender {} attaches files to {}", job.from, job.id),
            "Send your files on a reply job instead: kind \"result\" or \"comment\".",
        ));
    }
    if job.kind == Kind::Notice {
        return Err(ApiError::new(
            Code::AttachmentsLocked,
            format!("job {} is a notice, which carries no files", job.id),
            "Send a message instead (kind \"message\") and attach the files to it while it is sent.",
        ));
    }
    if job.state != State::Sent {
        return Err(ApiError::new(
            Code::AttachmentsLocked,
            format!("job {} is {}, its attachments are fixed", job.id, job.state.as_str()),
            "Send the file on a new job (kind \"comment\" with reply_to).",
        ));
    }
    let others = job.attachments.iter().filter(|a| a.name != name.as_str());
    let (count, used) = others.fold((0usize, 0u64), |(c, u), a| (c + 1, u + a.size.unsigned_abs()));
    if count >= limits::FILES_PER_JOB {
        return Err(ApiError::new(
            Code::TooLarge,
            format!("job {} already has {} attachments", job.id, limits::FILES_PER_JOB),
            format!(
                "At most {} files per job: pack them in an archive or a git bundle.",
                limits::FILES_PER_JOB
            ),
        ));
    }
    let left = limits::JOB_FILE_BYTES.saturating_sub(used);
    Ok(Budget {
        max_bytes: left.min(limits::FILE_BYTES),
    })
}

fn too_large_for_job(budget: Budget) -> ApiError {
    ApiError::new(
        Code::TooLarge,
        format!("upload is over the {} bytes this job can still take", budget.max_bytes),
        format!(
            "Limits: {} MiB per file, {} MiB per job. Split the file or send a git bundle.",
            limits::FILE_BYTES >> 20,
            limits::JOB_FILE_BYTES >> 20
        ),
    )
}

fn not_found(id: JobId) -> ApiError {
    ApiError::new(
        Code::NotFound,
        format!("job {id} not found"),
        "List your jobs with GET /inbox or GET /outbox; cancelled jobs are deleted.",
    )
}

fn not_yours(job: &Job) -> ApiError {
    ApiError::new(
        Code::NotYours,
        format!("job {} is between {} and {}", job.id, job.from, job.to),
        "Only the sender and the recipient can see or change a job.",
    )
}

/// Moves `job` and bumps its `updated_seq`. Returns whether the state changed.
fn apply_move(tx: &Transaction<'_>, job: &Job, mv: Move, reason: Option<&str>, now: i64) -> ApiResult<bool> {
    match step(job.state, mv) {
        Step::Repeat => Ok(false),
        Step::Refused => {
            let id = job.id;
            let hint = match (mv, job.state) {
                (Move::Accept, State::Sent) => format!("Receive it first: POST /jobs/{id}/receive or GET /jobs/{id}."),
                (Move::Finish(_), State::Sent | State::Received) => {
                    format!("Accept {id} first (POST /jobs/{id}/accept), then send the result.")
                }
                _ => format!("Nothing to do. Read it with GET /jobs/{id}."),
            };
            Err(ApiError::new(
                Code::BadTransition,
                format!("job {id} is {}, cannot {}", job.state.as_str(), mv.verb()),
                hint,
            ))
        }
        Step::To(state) => {
            set_state(tx, job.id, state, reason, now)?;
            Ok(true)
        }
    }
}

/// Stores a new state and bumps `updated_seq`, which wakes the sender's outbox
/// wait. Returns the new `updated_seq`.
fn set_state(tx: &Transaction<'_>, id: JobId, state: State, reason: Option<&str>, now: i64) -> ApiResult<i64> {
    let updated_seq = bump_counter(tx, "updated")?;
    tx.execute(
        "UPDATE jobs SET state = ?2, updated_at = ?3, updated_seq = ?4, reason = coalesce(?5, reason) WHERE id = ?1",
        params![id.get(), state.as_str(), now, updated_seq, reason],
    )?;
    Ok(updated_seq)
}

/// Databases made before notices, messages and reports lack `priority` and
/// `expects_reply`. Their tasks expected a reply all along, so they get `expects_reply = 1`.
fn add_reply_columns(conn: &Connection) -> rusqlite::Result<()> {
    let present: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('jobs') WHERE name = 'priority'",
        [],
        |row| row.get(0),
    )?;
    if present > 0 {
        return Ok(());
    }
    conn.execute_batch(
        "BEGIN;
         ALTER TABLE jobs ADD COLUMN priority TEXT NOT NULL DEFAULT 'normal';
         ALTER TABLE jobs ADD COLUMN expects_reply INTEGER NOT NULL DEFAULT 0;
         UPDATE jobs SET expects_reply = 1 WHERE kind = 'task';
         COMMIT;",
    )
}

/// Databases made before the model hint and capabilities lack those columns.
/// Both are nullable and null means "none", so old rows need no update.
fn add_hint_columns(conn: &Connection) -> rusqlite::Result<()> {
    for (table, column) in [("jobs", "model"), ("agents", "capabilities")] {
        let present: i64 = conn.query_row(
            "SELECT count(*) FROM pragma_table_info(?1) WHERE name = ?2",
            [table, column],
            |row| row.get(0),
        )?;
        if present == 0 {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} TEXT;"))?;
        }
    }
    Ok(())
}

fn bump_counter(conn: &Connection, name: &str) -> ApiResult<i64> {
    Ok(conn.query_row(
        "INSERT INTO counters (name, value) VALUES (?1, 1) \
         ON CONFLICT (name) DO UPDATE SET value = value + 1 RETURNING value",
        [name],
        |row| row.get(0),
    )?)
}

fn load_job(conn: &Connection, id: JobId, view: View) -> ApiResult<Job> {
    let columns = match view {
        View::Full => JOB_COLUMNS,
        View::Summary => SUMMARY_COLUMNS,
    };
    let job = conn
        .query_row(
            &format!("SELECT {columns} FROM jobs WHERE id = ?1"),
            [id.get()],
            job_from_row,
        )
        .optional()?;
    let mut job = job.ok_or_else(|| not_found(id))?;
    job.attachments = load_attachments(conn, id)?;
    Ok(job)
}

fn load_attachments(conn: &Connection, id: JobId) -> ApiResult<Vec<Attachment>> {
    let mut stmt = conn.prepare_cached("SELECT name, size, sha256 FROM attachments WHERE job = ?1 ORDER BY name")?;
    let rows = stmt.query_map([id.get()], |row| {
        Ok(Attachment {
            name: row.get(0)?,
            size: row.get(1)?,
            sha256: row.get(2)?,
        })
    })?;
    rows.map(|r| r.map_err(ApiError::from)).collect()
}

/// Text read back from our own database; a parse failure means corruption, not bad input.
struct Parsed<T>(T);

impl<T: FromStr<Err = String>> FromSql for Parsed<T> {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        text.parse()
            .map(Parsed)
            .map_err(|e: String| FromSqlError::Other(e.into()))
    }
}

/// JSON column, decoded with serde.
struct JsonCol<T>(T);

impl<T: for<'de> Deserialize<'de>> FromSql for JsonCol<T> {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        serde_json::from_str(value.as_str()?)
            .map(JsonCol)
            .map_err(|e| FromSqlError::Other(e.into()))
    }
}

fn job_from_row(row: &Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        id: JobId::new(row.get(0)?),
        from: row.get(1)?,
        to: row.get(2)?,
        kind: row.get::<_, Parsed<Kind>>(3)?.0,
        title: row.get(4)?,
        body: row.get(5)?,
        body_size: row.get(6)?,
        tags: row.get::<_, JsonCol<Vec<String>>>(7)?.0,
        origin: row.get::<_, Option<JsonCol<Origin>>>(8)?.map(|c| c.0),
        reply_to: row.get::<_, Option<i64>>(9)?.map(JobId::new),
        outcome: row.get(10)?,
        reason: row.get(11)?,
        state: row.get::<_, Parsed<State>>(12)?.0,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
        seq: row.get(15)?,
        updated_seq: row.get(16)?,
        priority: row.get::<_, Parsed<Priority>>(17)?.0,
        expects_reply: row.get(18)?,
        model: row.get(19)?,
        attachments: Vec::new(),
    })
}

/// SHA-256 of a request body, used to tell an idempotent retry from a different request.
pub fn body_hash(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static DIRS: AtomicUsize = AtomicUsize::new(0);

    /// A fresh data directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let n = DIRS.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("racking-store-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn agent(name: &str) -> AgentName {
        name.parse().unwrap_or_else(|e: String| panic!("{e}"))
    }

    fn task(to: &str) -> NewJob {
        NewJob {
            to: agent(to),
            kind: Kind::Task,
            title: "Run the probe".to_owned(),
            body: "Run it.".to_owned(),
            tags: vec!["decanter".to_owned()],
            origin: Some(Origin {
                repo: "decanter".to_owned(),
                id: "873".to_owned(),
                url: None,
            }),
            reply_to: None,
            outcome: None,
            priority: Priority::Normal,
            expects_reply: true,
            model: None,
        }
    }

    fn delivery(kind: Kind, to: &str, body: &str) -> NewJob {
        NewJob {
            kind,
            title: "Hello".to_owned(),
            body: body.to_owned(),
            origin: None,
            expects_reply: false,
            ..task(to)
        }
    }

    fn state_in_outbox(s: &Store, me: &AgentName, id: JobId) -> Option<State> {
        let out = s.outbox(me, Seq::new(0), 50).expect("outbox");
        out.jobs.iter().find(|j| j.id == id).map(|j| j.state)
    }

    #[test]
    fn peek_lists_without_delivering() {
        let (_dir, s) = store();
        let (linux, windows) = (agent("linux"), agent("windows"));
        let m = s
            .create_job(&linux, delivery(Kind::Message, "windows", "hi"))
            .expect("message");
        let before = s.outbox(&linux, Seq::new(0), 50).expect("outbox").next;
        let peeked = s
            .inbox_with(&windows, Seq::new(0), None, 50, Delivery::Peek)
            .expect("peek");
        assert_eq!(
            peeked.jobs.iter().map(|j| (j.id, j.state)).collect::<Vec<_>>(),
            [(m.id, State::Sent)]
        );
        assert_eq!(peeked.next, m.seq);
        assert!(
            s.outbox(&linux, Seq::new(before), 50).expect("outbox").jobs.is_empty(),
            "a peek changes nothing the sender sees"
        );
        let read = s.inbox(&windows, Seq::new(0), None, 50).expect("inbox");
        assert_eq!(read.jobs[0].state, State::Received, "a normal read still delivers it");
    }

    #[test]
    fn delivery_kinds_end_received_on_inbox_read() {
        let (_dir, s) = store();
        let (linux, windows) = (agent("linux"), agent("windows"));
        let m = s
            .create_job(&linux, delivery(Kind::Message, "windows", "hi\nthere"))
            .expect("message");
        let n = s
            .create_job(&linux, delivery(Kind::Notice, "windows", "build is red"))
            .expect("notice");
        let r = s
            .create_job(&linux, delivery(Kind::Report, "windows", &"r".repeat(300)))
            .expect("report");
        assert_eq!((m.state, n.state, r.state), (State::Sent, State::Sent, State::Sent));
        let before = s.outbox(&linux, Seq::new(0), 50).expect("outbox").next;

        let inbox = s.inbox(&windows, Seq::new(0), None, 50).expect("inbox");
        assert!(inbox.jobs.iter().all(|j| j.state == State::Received));
        let json = inbox.to_json();
        assert_eq!(json["jobs"][0]["body"], "hi\nthere", "a short message comes whole");
        assert_eq!(json["jobs"][0]["body_truncated"], false);
        assert_eq!(json["jobs"][1]["body"], "build is red");
        assert!(
            json["jobs"][2].get("body").is_none(),
            "a report comes as a summary only"
        );
        assert_eq!(json["jobs"][2]["body_truncated"], true);
        assert_eq!(json["jobs"][2]["body_preview"].as_str().map(str::len), Some(200));
        assert_eq!(json["jobs"][2]["priority"], "normal");

        let after = s.outbox(&linux, Seq::new(before), 50).expect("outbox");
        assert_eq!(after.jobs.len(), 3, "the sender's outbox cursor moves on delivery");
        let again = s.inbox(&windows, Seq::new(0), None, 50).expect("inbox");
        assert_eq!(
            again.jobs.iter().map(|j| j.updated_seq).collect::<Vec<_>>(),
            inbox.jobs.iter().map(|j| j.updated_seq).collect::<Vec<_>>(),
            "a repeated read changes nothing"
        );
        assert!(
            s.outbox(&linux, Seq::new(after.next), 50)
                .expect("outbox")
                .jobs
                .is_empty()
        );

        for id in [m.id, n.id, r.id] {
            for mv in [Move::Receive, Move::Accept, Move::Reject, Move::Finish(Outcome::Done)] {
                let err = s.move_job(&windows, id, mv, Some("no")).err().map(|e| e.code);
                assert_eq!(err, Some(Code::BadTransition), "{id} {mv:?}");
            }
        }
        let err = s.cancel_job(&linux, m.id).err().map(|e| e.code);
        assert_eq!(err, Some(Code::BadTransition), "delivered, too late to cancel");
        let fresh = s
            .create_job(&linux, delivery(Kind::Message, "windows", "oops"))
            .expect("message");
        s.cancel_job(&linux, fresh.id).expect("cancel while sent");

        s.db()
            .execute(
                "UPDATE jobs SET updated_at = updated_at - ?1",
                [limits::CLOSED_JOB_TTL_SECS + 1],
            )
            .expect("age jobs");
        let purged = s.purge(clock::now()).expect("purge");
        assert_eq!(purged.len(), 3, "delivered jobs without a reply expected are closed");
    }

    #[test]
    fn replies_to_delivery_kinds() {
        let (_dir, s) = store();
        let (linux, windows) = (agent("linux"), agent("windows"));
        let ask = NewJob {
            expects_reply: true,
            ..delivery(Kind::Message, "windows", "which branch?")
        };
        let ask = s.create_job(&linux, ask).expect("message");
        let plain = s
            .create_job(&linux, delivery(Kind::Message, "windows", "fyi"))
            .expect("message");
        let notice = s
            .create_job(&linux, delivery(Kind::Notice, "windows", "deploying"))
            .expect("notice");
        s.inbox(&windows, Seq::new(0), None, 50).expect("deliver");

        for kind in [Kind::Comment, Kind::Result] {
            let reply = NewJob {
                reply_to: Some(ask.id),
                ..delivery(kind, "linux", "x")
            };
            let err = s.create_job(&windows, reply).err().map(|e| e.code);
            assert_eq!(err, Some(Code::BadTransition), "{kind:?} to a message");
        }
        let reply = NewJob {
            reply_to: Some(notice.id),
            ..delivery(Kind::Message, "linux", "ok")
        };
        let err = s.create_job(&windows, reply).err().map(|e| e.code);
        assert_eq!(err, Some(Code::BadTransition), "a notice takes no reply");

        let follow_up = NewJob {
            reply_to: Some(ask.id),
            ..delivery(Kind::Message, "windows", "also: main")
        };
        s.create_job(&linux, follow_up)
            .expect("the sender may add to its own message");
        assert_eq!(state_in_outbox(&s, &linux, ask.id), Some(State::Received));

        let before = s.outbox(&linux, Seq::new(0), 50).expect("outbox").next;
        let answer = NewJob {
            reply_to: Some(ask.id),
            ..delivery(Kind::Message, "linux", "main")
        };
        s.create_job(&windows, answer).expect("answer");
        let woke = s.outbox(&linux, Seq::new(before), 50).expect("outbox");
        assert_eq!(
            woke.jobs.first().map(|j| (j.id, j.state)),
            Some((ask.id, State::Answered))
        );

        let answer = NewJob {
            reply_to: Some(plain.id),
            ..delivery(Kind::Message, "linux", "thanks")
        };
        s.create_job(&windows, answer).expect("answer");
        assert_eq!(
            state_in_outbox(&s, &linux, plain.id),
            Some(State::Received),
            "no reply was expected, so it stays received"
        );
    }

    #[test]
    fn attachments_by_kind() {
        let (_dir, s) = store();
        let (linux, windows) = (agent("linux"), agent("windows"));
        let name: AttachmentName = "a.txt".parse().expect("name");
        let sha = Sha256Hex::of_digest(&Sha256::digest(b"hi"));
        let upload = |id| {
            s.put_attachment(&linux, id, &name, &mut &b"hi"[..], &sha, None)
                .err()
                .map(|e| e.code)
        };
        let notice = s
            .create_job(&linux, delivery(Kind::Notice, "windows", "n"))
            .expect("notice");
        assert_eq!(upload(notice.id), Some(Code::AttachmentsLocked));
        let message = s
            .create_job(&linux, delivery(Kind::Message, "windows", "m"))
            .expect("message");
        let report = s
            .create_job(&linux, delivery(Kind::Report, "windows", "r"))
            .expect("report");
        assert_eq!((upload(message.id), upload(report.id)), (None, None));

        let inbox = s.inbox(&windows, Seq::new(0), None, 50).expect("inbox");
        let files = inbox.jobs.iter().map(|j| j.attachments.len()).collect::<Vec<_>>();
        assert_eq!(files, [0, 1, 1], "the inbox lists attachment summaries");
        assert_eq!(
            upload(message.id),
            Some(Code::AttachmentsLocked),
            "fixed once delivered"
        );
    }

    #[test]
    fn inline_bodies_up_to_four_kib() {
        let (_dir, s) = store();
        let (linux, windows) = (agent("linux"), agent("windows"));
        for body in [
            "é".repeat(limits::INLINE_BYTES / 2),
            format!("{}x", "é".repeat(limits::INLINE_BYTES / 2)),
        ] {
            s.create_job(&linux, delivery(Kind::Message, "windows", &body))
                .expect("message");
        }
        s.create_job(
            &linux,
            delivery(Kind::Message, "windows", "x\u{1F600}".repeat(1500).as_str()),
        )
        .expect("message");
        let json = s.inbox(&windows, Seq::new(0), None, 50).expect("inbox").to_json();
        assert_eq!(
            json["jobs"][0]["body"].as_str().map(str::len),
            Some(limits::INLINE_BYTES)
        );
        assert!(json["jobs"][1].get("body").is_none(), "4 KiB + 1 comes as a preview");
        assert_eq!(json["jobs"][1]["body_truncated"], true);
        assert!(json["jobs"][2].get("body").is_none());
        let preview = json["jobs"][2]["body_preview"].as_str().unwrap_or_default();
        assert_eq!(preview.chars().count(), 200, "the preview counts chars, not bytes");
    }

    #[test]
    fn old_databases_gain_the_reply_columns() {
        let dir = TempDir::new();
        fs::create_dir_all(&dir.0).expect("dir");
        let conn = Connection::open(dir.0.join("racking.db")).expect("db");
        conn.execute_batch(
            "CREATE TABLE jobs (id INTEGER PRIMARY KEY AUTOINCREMENT, from_agent TEXT NOT NULL, \
             to_agent TEXT NOT NULL, kind TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL, \
             tags TEXT NOT NULL, origin TEXT, reply_to INTEGER, outcome TEXT, reason TEXT, state TEXT NOT NULL, \
             created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, seq INTEGER NOT NULL, \
             updated_seq INTEGER NOT NULL) STRICT;
             INSERT INTO jobs VALUES (1, 'linux', 'windows', 'task', 't', 'b', '[]', NULL, NULL, NULL, NULL, \
             'sent', 0, 0, 1, 1);",
        )
        .expect("old schema");
        drop(conn);
        let s = Store::open(&dir.0).expect("open");
        let job = s.get_job(&agent("linux"), JobId::new(1)).expect("job");
        assert_eq!((job.priority, job.expects_reply), (Priority::Normal, true));
        assert_eq!(job.model, None);
        drop(s);
        Store::open(&dir.0).expect("opening twice is fine");
    }

    #[test]
    fn old_databases_gain_the_model_and_capabilities_columns() {
        let dir = TempDir::new();
        fs::create_dir_all(&dir.0).expect("dir");
        let conn = Connection::open(dir.0.join("racking.db")).expect("db");
        conn.execute_batch(
            "CREATE TABLE jobs (id INTEGER PRIMARY KEY AUTOINCREMENT, from_agent TEXT NOT NULL, \
             to_agent TEXT NOT NULL, kind TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL, \
             tags TEXT NOT NULL, origin TEXT, reply_to INTEGER, outcome TEXT, reason TEXT, state TEXT NOT NULL, \
             created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, seq INTEGER NOT NULL, \
             updated_seq INTEGER NOT NULL, priority TEXT NOT NULL DEFAULT 'normal', \
             expects_reply INTEGER NOT NULL DEFAULT 0) STRICT;
             INSERT INTO jobs VALUES (1, 'linux', 'windows', 'task', 't', 'b', '[]', NULL, NULL, NULL, NULL, \
             'sent', 0, 0, 1, 1, 'high', 1);
             CREATE TABLE agents (name TEXT PRIMARY KEY, last_seen INTEGER, about TEXT, rack_version TEXT, \
             os TEXT) STRICT;
             INSERT INTO agents VALUES ('windows', 100, 'desk PC', '0.1.0', 'windows');",
        )
        .expect("schema before the model hint");
        drop(conn);
        let s = Store::open(&dir.0).expect("open");
        let (linux, windows) = (agent("linux"), agent("windows"));
        let old = s.get_job(&linux, JobId::new(1)).expect("old job");
        assert_eq!((old.priority, old.model.as_deref()), (Priority::High, None));
        assert_eq!(old.to_json(View::Full)["model"], Value::Null);
        let rows = s.load_agents().expect("agents");
        let row = rows.iter().find(|r| r.name == "windows").expect("row");
        assert_eq!(
            (row.info.about.as_deref(), row.info.capabilities.len()),
            (Some("desk PC"), 0)
        );
        let new = NewJob {
            model: Some("opus".to_owned()),
            ..task("windows")
        };
        let job = s.create_job(&linux, new).expect("job with a model");
        assert_eq!(job.to_json(View::Full)["model"], "opus");
        let inbox = s.inbox(&windows, Seq::new(0), None, 10).expect("inbox").to_json();
        assert_eq!(inbox["jobs"][1]["model"], "opus", "summaries carry it too");
        drop(s);
        Store::open(&dir.0).expect("opening twice is fine");
    }

    fn store() -> (TempDir, Store) {
        let dir = TempDir::new();
        let store = Store::open(&dir.0).expect("open store");
        (dir, store)
    }

    #[test]
    fn graph_moves() {
        assert_eq!(step(State::Sent, Move::Receive), Step::To(State::Received));
        assert_eq!(step(State::Received, Move::Receive), Step::Repeat);
        assert_eq!(step(State::Sent, Move::Accept), Step::Refused);
        assert_eq!(step(State::Received, Move::Accept), Step::To(State::Accepted));
        assert_eq!(step(State::Accepted, Move::Accept), Step::Repeat);
        assert_eq!(step(State::Sent, Move::Reject), Step::To(State::Rejected));
        assert_eq!(step(State::Received, Move::Reject), Step::To(State::Rejected));
        assert_eq!(step(State::Accepted, Move::Reject), Step::Refused);
        assert_eq!(
            step(State::Accepted, Move::Finish(Outcome::Done)),
            Step::To(State::Done)
        );
        assert_eq!(
            step(State::Accepted, Move::Finish(Outcome::Failed)),
            Step::To(State::Failed)
        );
        assert_eq!(step(State::Done, Move::Finish(Outcome::Done)), Step::Repeat);
        assert_eq!(step(State::Done, Move::Finish(Outcome::Failed)), Step::Refused);
        assert_eq!(step(State::Done, Move::Accept), Step::Refused);
        assert_eq!(step(State::Rejected, Move::Accept), Step::Refused);
    }

    #[test]
    fn job_lifecycle_and_parties() {
        let (_dir, s) = store();
        let (linux, windows, other) = (agent("linux"), agent("windows"), agent("mac"));
        let j = s.create_job(&linux, task("windows")).expect("create");
        assert_eq!((j.id.get(), j.seq, j.state), (1, 1, State::Sent));

        let err = s.move_job(&linux, j.id, Move::Accept, None).err().map(|e| e.code);
        assert_eq!(err, Some(Code::NotYours), "sender cannot accept");
        let err = s.get_job(&other, j.id).err().map(|e| e.code);
        assert_eq!(err, Some(Code::NotYours), "third agent cannot read");
        let err = s.move_job(&windows, j.id, Move::Accept, None).err().map(|e| e.code);
        assert_eq!(err, Some(Code::BadTransition), "accept needs receive first");

        assert_eq!(
            s.get_job(&linux, j.id).map(|j| j.state).ok(),
            Some(State::Sent),
            "sender read keeps sent"
        );
        assert_eq!(s.get_job(&windows, j.id).map(|j| j.state).ok(), Some(State::Received));
        assert_eq!(
            s.move_job(&windows, j.id, Move::Receive, None).map(|j| j.state).ok(),
            Some(State::Received)
        );
        assert_eq!(
            s.move_job(&windows, j.id, Move::Accept, None).map(|j| j.state).ok(),
            Some(State::Accepted)
        );
        let err = s.cancel_job(&linux, j.id).err().map(|e| e.code);
        assert_eq!(err, Some(Code::BadTransition), "cannot cancel once received");

        let mut result = task("linux");
        result.kind = Kind::Result;
        result.reply_to = Some(j.id);
        result.outcome = Some(Outcome::Failed);
        let r = s.create_job(&windows, result).expect("result");
        assert_eq!((r.seq, r.outcome.as_deref()), (1, Some("failed")), "seq is per inbox");
        assert_eq!(s.get_job(&linux, j.id).map(|j| j.state).ok(), Some(State::Failed));

        let out = s.outbox(&linux, Seq::new(0), 10).expect("outbox");
        assert_eq!(out.jobs.first().map(|j| j.state), Some(State::Failed));
        assert_eq!(out.next, out.jobs.first().map_or(0, |j| j.updated_seq));
        let again = s.outbox(&linux, Seq::new(out.next), 10).expect("outbox");
        assert!(again.jobs.is_empty());
        assert_eq!(again.next, out.next);
    }

    #[test]
    fn reply_rules() {
        let (_dir, s) = store();
        let (linux, windows) = (agent("linux"), agent("windows"));
        let j = s.create_job(&linux, task("windows")).expect("create");
        let mut result = task("linux");
        result.kind = Kind::Result;
        result.reply_to = Some(j.id);
        let err = s.create_job(&linux, result).err().map(|e| e.code);
        assert_eq!(err, Some(Code::BadRequest), "a reply goes to the other party");
        let mut result = task("windows");
        result.kind = Kind::Result;
        result.reply_to = Some(j.id);
        let err = s.create_job(&linux, result).err().map(|e| e.code);
        assert_eq!(err, Some(Code::NotYours), "the sender cannot report a result");
        let mut result = task("linux");
        result.kind = Kind::Result;
        result.reply_to = Some(j.id);
        let err = s.create_job(&windows, result).err().map(|e| e.code);
        assert_eq!(err, Some(Code::BadTransition), "result needs an accepted job");
        assert_eq!(
            s.inbox(&linux, Seq::new(0), None, 10).map(|l| l.jobs.len()).ok(),
            Some(0),
            "nothing stored"
        );
    }

    #[test]
    fn cancel_deletes() {
        let (_dir, s) = store();
        let linux = agent("linux");
        let j = s.create_job(&linux, task("windows")).expect("create");
        let name: AttachmentName = "a.txt".parse().expect("name");
        let sha = Sha256Hex::of_digest(&Sha256::digest(b"hi"));
        s.put_attachment(&linux, j.id, &name, &mut &b"hi"[..], &sha, None)
            .expect("upload");
        assert!(s.job_dir(j.id).join("a.txt").exists());
        s.cancel_job(&linux, j.id).expect("cancel");
        assert!(!s.job_dir(j.id).exists());
        assert_eq!(s.get_job(&linux, j.id).err().map(|e| e.code), Some(Code::NotFound));
        let j2 = s.create_job(&linux, task("windows")).expect("create");
        assert_eq!(j2.id.get(), 2, "ids are never reused");
    }

    #[test]
    fn checksum_and_limits() {
        let (_dir, s) = store();
        let (linux, windows) = (agent("linux"), agent("windows"));
        let j = s.create_job(&linux, task("windows")).expect("create");
        let name: AttachmentName = "probe.bat".parse().expect("name");
        let good = Sha256Hex::of_digest(&Sha256::digest(b"echo hi"));
        let bad = Sha256Hex::of_digest(&Sha256::digest(b"other"));

        let err = s
            .put_attachment(&linux, j.id, &name, &mut &b"echo hi"[..], &bad, None)
            .err()
            .map(|e| e.code);
        assert_eq!(err, Some(Code::ChecksumMismatch));
        assert!(
            !s.job_dir(j.id).join("probe.bat").exists(),
            "a bad upload leaves nothing"
        );
        assert_eq!(
            fs::read_dir(&s.tmp).map(Iterator::count).ok(),
            Some(0),
            "temp file removed"
        );

        let att = s
            .put_attachment(&linux, j.id, &name, &mut &b"echo hi"[..], &good, None)
            .expect("upload");
        assert_eq!((att.size, att.sha256.as_str()), (7, good.as_str()));
        let err = s
            .put_attachment(&windows, j.id, &name, &mut &b"echo hi"[..], &good, None)
            .err()
            .map(|e| e.code);
        assert_eq!(err, Some(Code::NotYours), "recipient cannot upload");

        let mut dl = s.get_attachment(&windows, j.id, &name).expect("download");
        let mut bytes = Vec::new();
        dl.file.read_to_end(&mut bytes).expect("read");
        assert_eq!((bytes.as_slice(), dl.sha256.as_str()), (&b"echo hi"[..], good.as_str()));

        s.get_job(&windows, j.id).expect("receive");
        let err = s
            .put_attachment(&linux, j.id, &name, &mut &b"echo hi"[..], &good, None)
            .err()
            .map(|e| e.code);
        assert_eq!(err, Some(Code::AttachmentsLocked));
    }

    #[test]
    fn upload_size_limit() {
        let (_dir, s) = store();
        let linux = agent("linux");
        let j = s.create_job(&linux, task("windows")).expect("create");
        let name: AttachmentName = "big.bin".parse().expect("name");
        let sha = Sha256Hex::of_digest(&Sha256::digest(b""));
        let mut big = io::repeat(0).take(limits::FILE_BYTES + 1);
        let err = s
            .put_attachment(&linux, j.id, &name, &mut big, &sha, None)
            .err()
            .map(|e| e.code);
        assert_eq!(err, Some(Code::TooLarge));
    }

    #[test]
    fn idempotency_keys() {
        let (_dir, s) = store();
        let linux = agent("linux");
        let windows = agent("windows");
        assert_eq!(
            s.claim_key(&linux, "k1", "POST", "/jobs", "h1").ok(),
            Some(Claim::Fresh)
        );
        assert_eq!(
            s.claim_key(&linux, "k1", "POST", "/jobs", "h1").ok(),
            Some(Claim::InFlight)
        );
        s.finish_key(&linux, "k1", 201, "{\"id\":\"j1\"}").expect("finish");
        assert_eq!(
            s.claim_key(&linux, "k1", "POST", "/jobs", "h1").ok(),
            Some(Claim::Replay {
                status: 201,
                body: "{\"id\":\"j1\"}".to_owned()
            })
        );
        assert_eq!(
            s.claim_key(&linux, "k1", "POST", "/jobs", "h2").ok(),
            Some(Claim::Conflict)
        );
        assert_eq!(
            s.claim_key(&linux, "k1", "PUT", "/jobs", "h1").ok(),
            Some(Claim::Conflict)
        );
        assert_eq!(
            s.claim_key(&windows, "k1", "POST", "/jobs", "h2").ok(),
            Some(Claim::Fresh),
            "keys are per agent"
        );
        s.release_key(&windows, "k1").expect("release");
        assert_eq!(
            s.claim_key(&windows, "k1", "POST", "/jobs", "h3").ok(),
            Some(Claim::Fresh)
        );

        s.db()
            .execute(
                "UPDATE idempotency SET created_at = created_at - ?1",
                [limits::IDEMPOTENCY_TTL_SECS + 1],
            )
            .expect("age keys");
        assert_eq!(
            s.claim_key(&linux, "k1", "POST", "/jobs", "h2").ok(),
            Some(Claim::Fresh),
            "expired after 24 h"
        );
    }

    #[test]
    fn wait_wakes_on_change() {
        let (_dir, s) = store();
        let s = std::sync::Arc::new(s);
        let (linux, windows) = (agent("linux"), agent("windows"));
        let started = Instant::now();
        let waiter = {
            let s = std::sync::Arc::clone(&s);
            std::thread::spawn(move || {
                let deadline = Instant::now() + std::time::Duration::from_secs(10);
                s.wait(deadline, |st| st.inbox(&windows, Seq::new(0), None, 10))
                    .map(|l| l.jobs.len())
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        s.create_job(&linux, task("windows")).expect("create");
        let got = waiter.join().expect("join").expect("wait");
        assert_eq!(got, 1);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "woken, not timed out"
        );
    }

    #[test]
    fn presence_rows_round_trip() {
        let (_dir, s) = store();
        let windows = agent("windows");
        let seen = |at, os: Option<&str>| presence::Flush {
            at,
            rack_version: Some("0.1.0".to_owned()),
            os: os.map(str::to_owned),
        };
        s.save_seen(&windows, &seen(100, Some("windows"))).expect("seen");
        s.save_seen(&windows, &seen(50, None)).expect("older write");
        let info = presence::Info {
            about: Some("desk PC".to_owned()),
            rack_version: Some("0.1.0".to_owned()),
            os: None,
            capabilities: vec!["kb".to_owned(), "nvidia".to_owned()],
        };
        s.save_agent_info(&windows, &info).expect("info");
        let rows = s.load_agents().expect("load");
        let row = rows.iter().find(|r| r.name == "windows").expect("row");
        assert_eq!(row.last_seen, Some(100), "last_seen never goes back");
        assert_eq!(
            (
                row.info.about.as_deref(),
                row.info.rack_version.as_deref(),
                row.info.os.as_deref()
            ),
            (Some("desk PC"), Some("0.1.0"), None)
        );
        assert_eq!(row.info.capabilities, ["kb", "nvidia"]);
        let cleared = presence::Info {
            capabilities: Vec::new(),
            ..info
        };
        s.save_agent_info(&windows, &cleared).expect("clear");
        let rows = s.load_agents().expect("load");
        let row = rows.iter().find(|r| r.name == "windows").expect("row");
        assert!(row.info.capabilities.is_empty());
    }
}
