//! Tool arguments. Values that go into URLs or bound a wait are newtypes
//! checked while the arguments are deserialized, so handlers only see valid
//! ones; unknown fields are refused so a misspelt option is never ignored.
//! Job fields the server validates (recipient, title, tags) are left to it.

use rack::client;
use serde::Deserialize;

use crate::tools::describe;

#[derive(Deserialize)]
#[serde(try_from = "String")]
pub struct JobId(String);

impl JobId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for JobId {
    type Error = String;

    fn try_from(id: String) -> Result<Self, String> {
        client::job_id(&id).map_err(|e| describe(&e))?;
        Ok(Self(id))
    }
}

#[derive(Deserialize)]
#[serde(try_from = "String")]
pub struct AttachmentName(String);

impl AttachmentName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AttachmentName {
    type Error = String;

    fn try_from(name: String) -> Result<Self, String> {
        client::download_name(&name).map_err(|e| describe(&e))?;
        Ok(Self(name))
    }
}

/// Seconds a wait may block. Capped well under the server's 55 so a call ends
/// before MCP clients that enforce a per-call timeout give up on it.
#[derive(Deserialize)]
#[serde(try_from = "u64")]
pub struct WaitSecs(u64);

impl WaitSecs {
    pub const DEFAULT: u64 = 20;
    pub const MAX: u64 = 25;

    pub fn get(&self) -> u64 {
        self.0
    }
}

impl Default for WaitSecs {
    fn default() -> Self {
        Self(Self::DEFAULT)
    }
}

impl TryFrom<u64> for WaitSecs {
    type Error = String;

    fn try_from(secs: u64) -> Result<Self, String> {
        if (1..=Self::MAX).contains(&secs) {
            Ok(Self(secs))
        } else {
            Err(format!("timeout must be 1 to {} seconds", Self::MAX))
        }
    }
}

/// A job state filter; it goes into a query string.
#[derive(Deserialize)]
#[serde(try_from = "String")]
pub struct StateName(String);

impl StateName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for StateName {
    type Error = String;

    fn try_from(state: String) -> Result<Self, String> {
        if (1..=16).contains(&state.len()) && state.bytes().all(|b| b.is_ascii_lowercase()) {
            Ok(Self(state))
        } else {
            Err(format!("state `{state}` must be a state name such as received"))
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Task,
    Comment,
    Result,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Task => "task",
            Kind::Comment => "comment",
            Kind::Result => "result",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    Normal,
    High,
}

impl Priority {
    pub fn as_str(&self) -> &'static str {
        match self {
            Priority::Low => "low",
            Priority::Normal => "normal",
            Priority::High => "high",
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ResultOutcome {
    #[default]
    Done,
    Failed,
}

impl ResultOutcome {
    pub fn to_client(&self) -> client::Outcome {
        match self {
            ResultOutcome::Done => client::Outcome::Done,
            ResultOutcome::Failed => client::Outcome::Failed,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WaitOn {
    #[default]
    Inbox,
    Outbox,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Guide {
    #[serde(default)]
    pub changes: bool,
    pub command: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    pub repo: String,
    pub id: String,
    pub url: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendJob {
    pub to: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub origin: Option<Origin>,
    pub reply_to: Option<JobId>,
    pub kind: Option<Kind>,
    pub outcome: Option<ResultOutcome>,
    pub priority: Option<Priority>,
    pub model: Option<String>,
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default)]
    pub force: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Say {
    pub to: String,
    pub text: String,
    pub title: Option<String>,
    pub reply_to: Option<JobId>,
    pub expects_reply: Option<bool>,
    pub priority: Option<Priority>,
    pub model: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub attachments: Vec<String>,
}

/// Either `to` and `text` (a new question) or `resume` alone (wait again for
/// the reply to an earlier one); the handler checks which.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ask {
    pub to: Option<String>,
    pub text: Option<String>,
    pub resume: Option<JobId>,
    #[serde(default)]
    pub timeout: WaitSecs,
    pub priority: Option<Priority>,
    #[serde(default)]
    pub attachments: Vec<String>,
    #[serde(default)]
    pub since: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notify {
    pub to: String,
    pub text: String,
    pub title: Option<String>,
    pub priority: Option<Priority>,
}

/// The report text comes from `body` or from the file at `path`, exactly one.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub to: String,
    pub title: String,
    pub body: Option<String>,
    pub path: Option<String>,
    pub expects_reply: Option<bool>,
    pub priority: Option<Priority>,
    pub model: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub attachments: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inbox {
    #[serde(default)]
    pub since: u64,
    pub state: Option<StateName>,
    pub limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wait {
    #[serde(default)]
    pub since: u64,
    #[serde(default)]
    pub timeout: WaitSecs,
    #[serde(default)]
    pub on: WaitOn,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub job: JobId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reject {
    pub job: JobId,
    pub reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportResult {
    pub job: JobId,
    #[serde(default)]
    pub body: String,
    pub title: Option<String>,
    #[serde(default)]
    pub outcome: ResultOutcome,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outbox {
    #[serde(default)]
    pub since: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attach {
    pub job: JobId,
    pub path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetAttachment {
    pub job: JobId,
    pub name: AttachmentName,
    pub dir: String,
    #[serde(default)]
    pub overwrite: bool,
}

#[cfg(test)]
mod tests {
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};

    use super::*;

    fn parse<T: DeserializeOwned>(v: Value) -> Result<T, String> {
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    #[test]
    fn job_ids_are_checked() {
        assert_eq!(parse::<Job>(json!({"job": "j3"})).map(|a| a.job.0), Ok("j3".to_owned()));
        let err = parse::<Job>(json!({"job": "j3/../x"})).err().unwrap_or_default();
        assert!(err.contains("must look like j42"), "{err}");
        assert!(parse::<Job>(json!({})).is_err());
    }

    #[test]
    fn unknown_fields_are_refused() {
        let err = parse::<Job>(json!({"job": "j1", "force": true}))
            .err()
            .unwrap_or_default();
        assert!(err.contains("unknown field `force`"), "{err}");
    }

    #[test]
    fn wait_bounds_and_defaults() {
        let w: Wait = parse(json!({})).expect("defaults");
        assert_eq!((w.since, w.timeout.get()), (0, WaitSecs::DEFAULT));
        assert!(matches!(w.on, WaitOn::Inbox));
        assert!(parse::<Wait>(json!({"timeout": 25, "on": "outbox"})).is_ok());
        assert!(parse::<Wait>(json!({"timeout": 26})).is_err());
        assert!(parse::<Wait>(json!({"timeout": 0})).is_err());
        assert!(parse::<Wait>(json!({"since": -1})).is_err());
        assert!(parse::<Wait>(json!({"on": "both"})).is_err());
    }

    #[test]
    fn attachment_names_and_states() {
        let ok = json!({"job": "j1", "name": "probe.log", "dir": "in"});
        assert!(parse::<GetAttachment>(ok).is_ok_and(|a| !a.overwrite));
        assert!(parse::<GetAttachment>(json!({"job": "j1", "name": "../x", "dir": "in"})).is_err());
        assert!(parse::<GetAttachment>(json!({"job": "j1", "name": ".bashrc", "dir": "in"})).is_err());
        assert!(parse::<Inbox>(json!({"state": "received"})).is_ok());
        assert!(parse::<Inbox>(json!({"state": "x&since=0"})).is_err());
    }

    #[test]
    fn send_kinds() {
        let s: SendJob = parse(json!({"to": "windows", "title": "t"})).expect("minimal send");
        assert!(s.kind.is_none() && s.body.is_empty() && s.tags.is_empty());
        assert!(parse::<SendJob>(json!({"to": "w", "title": "t", "kind": "order"})).is_err());
        assert!(parse::<SendJob>(json!({"to": "w", "title": "t", "origin": {"repo": "r"}})).is_err());
        assert!(parse::<SendJob>(json!({"to": "w", "title": "t", "priority": "urgent"})).is_err());
        let s: SendJob = parse(json!({"to": "w", "title": "t", "model": "opus", "needs": ["nvidia"], "force": true}))
            .expect("model and needs");
        assert_eq!((s.model.as_deref(), s.needs.len(), s.force), (Some("opus"), 1, true));
        assert!(parse::<SendJob>(json!({"to": "w", "title": "t", "needs": "nvidia"})).is_err());
    }

    #[test]
    fn ask_arguments() {
        let a: Ask = parse(json!({"to": "w", "text": "hi"})).expect("question");
        assert_eq!((a.timeout.get(), a.since), (WaitSecs::DEFAULT, 0));
        assert!(parse::<Ask>(json!({"resume": "j4", "timeout": 25})).is_ok());
        assert!(parse::<Ask>(json!({"resume": "4"})).is_err());
        assert!(parse::<Ask>(json!({"to": "w", "text": "hi", "timeout": 26})).is_err());
    }

    #[test]
    fn say_notify_report() {
        let s: Say = parse(json!({"to": "w", "text": "hi"})).expect("minimal say");
        assert!(s.reply_to.is_none() && s.attachments.is_empty() && s.priority.is_none());
        assert!(parse::<Say>(json!({"to": "w", "text": "hi", "reply_to": "x"})).is_err());
        assert!(parse::<Notify>(json!({"to": "w", "text": "hi", "reply_to": "j1"})).is_err());
        assert!(parse::<Notify>(json!({"to": "w", "text": "hi", "priority": "high"})).is_ok());
        assert!(parse::<Report>(json!({"to": "w", "text": "hi"})).is_err());
        let r: Report = parse(json!({"to": "w", "title": "t", "path": "out/report.md"})).expect("report");
        assert!(r.body.is_none() && r.path.is_some());
    }
}
