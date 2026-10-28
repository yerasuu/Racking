//! `GET /threads/{id}` and `GET /jobs` for `rack thread`, `rack search` and their
//! MCP tools: the request paths and the one-line form of each job, so both
//! clients print a conversation the same way.

use std::fmt::Write as _;

use serde::Deserialize;

use crate::client::{Job, Result, job_id};
use crate::text::{self, CONTINUATION};

/// One job of a thread: the job as listed, and how many replies deep it is.
#[derive(Deserialize)]
pub struct ThreadJob {
    #[serde(flatten)]
    pub job: Job,
    #[serde(default)]
    pub depth: u32,
}

#[derive(Deserialize)]
pub struct Thread {
    pub root: String,
    pub jobs: Vec<ThreadJob>,
    /// Jobs of the thread between other agents: never shown, only counted.
    #[serde(default)]
    pub omitted: u64,
    /// The cursor of the rest, when the page stopped before the end.
    #[serde(default)]
    pub next: Option<String>,
}

#[derive(Deserialize)]
pub struct Search {
    pub jobs: Vec<Job>,
    #[serde(default)]
    pub next: Option<String>,
}

/// Whether thread entries carry whole bodies or inbox summaries.
#[derive(Clone, Copy)]
pub enum Bodies {
    Summary,
    Full,
}

/// Percent-encodes a query value as UTF-8, keeping only RFC 3986 unreserved
/// characters, so spaces, `&`, `%` and non-ASCII text reach the server as typed.
pub fn encode(value: &str) -> String {
    value.bytes().fold(String::with_capacity(value.len()), |mut out, b| {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
        out
    })
}

/// `/threads/JOB` with its options; `after` is the `next` of a previous page.
pub fn thread_path(job: &str, bodies: Bodies, limit: Option<u32>, after: Option<&str>) -> Result<String> {
    let mut path = format!("/threads/{}?", job_id(job)?);
    if matches!(bodies, Bodies::Full) {
        path.push_str("full=1&");
    }
    if let Some(n) = limit {
        let _ = write!(path, "limit={n}&");
    }
    if let Some(a) = after {
        let _ = write!(path, "after={}&", encode(a));
    }
    Ok(path.trim_end_matches(['&', '?']).to_owned())
}

/// The filters of a search; the server checks every value.
#[derive(Default)]
pub struct Filters<'a> {
    pub text: Option<&'a str>,
    pub from: Option<&'a str>,
    pub to: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub state: Option<&'a str>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub limit: Option<u32>,
    pub after: Option<&'a str>,
}

/// `/jobs?...` for `filters`, every value encoded.
pub fn search_path(filters: &Filters<'_>) -> String {
    let limit = filters.limit.map(|n| n.to_string());
    let pairs = [
        ("q", filters.text),
        ("from", filters.from),
        ("to", filters.to),
        ("kind", filters.kind),
        ("state", filters.state),
        ("since", filters.since),
        ("until", filters.until),
        ("limit", limit.as_deref()),
        ("after", filters.after),
    ];
    let mut path = "/jobs?".to_owned();
    for (name, value) in pairs {
        if let Some(v) = value {
            let _ = write!(path, "{name}={}&", encode(v));
        }
    }
    path.trim_end_matches(['&', '?']).to_owned()
}

/// `2026-10-28 12:00` from the server's `2026-10-28T12:00:00Z`; whatever else as it came.
fn minute(at: &str) -> String {
    match at.get(..16) {
        Some(head) if at.len() == 20 && at.as_bytes().get(10) == Some(&b'T') => head.replacen('T', " ", 1),
        _ => at.to_owned(),
    }
}

/// One job of a conversation, oldest-first friendly:
///
/// ```text
/// j3 message linux -> windows [re j2] 2026-10-28 12:00: Which branch?
///     The old one fails on Windows.
/// j4 task done linux [main] -> windows 2026-10-28 12:05: Run the probe
/// ```
///
/// A notice or message shows its text, further lines indented by `CONTINUATION`;
/// other kinds show their state and title, then their whole body indented when
/// it came (`--full`). Everything after the time was written by the sender.
pub fn line(job: &Job) -> String {
    let delivery = text::is_delivery(&job.kind);
    let mut out = format!("{} {}{}", job.id, job.kind, text::priority_tag(job));
    if !delivery {
        let _ = write!(out, " {}", job.state);
    }
    let _ = write!(out, " {}{} -> {}", job.from, text::role_tag(job), job.to);
    if let Some(r) = &job.reply_to {
        let _ = write!(out, " [re {r}]");
    }
    if let Some(at) = &job.created_at {
        let _ = write!(out, " {}", minute(at));
    }
    if !job.attachments.is_empty() {
        let _ = write!(out, " ({})", text::files(job.attachments.len()));
    }
    out.push_str(": ");
    let (head, rest) = if delivery && job.kind != "report" {
        let text = match (&job.body, &job.body_preview) {
            (Some(body), _) if !body.trim().is_empty() => body.as_str(),
            (_, Some(preview)) if !preview.trim().is_empty() => preview.as_str(),
            _ => job.title.as_str(),
        };
        let mut lines = text.trim_end().lines();
        (lines.next().unwrap_or_default().to_owned(), lines.collect::<Vec<_>>())
    } else {
        let body = job.body.as_deref().unwrap_or_default().trim_end();
        (job.title.clone(), body.lines().collect())
    };
    out.push_str(&head);
    for l in rest {
        let _ = write!(out, "\n{CONTINUATION}{}", l.trim_end_matches('\r'));
    }
    if job.body.is_none() && job.body_truncated {
        let _ = write!(out, " [truncated, {} in all]", text::size(job.body_size));
    }
    out
}

/// The lines after a thread's jobs: how many were hidden and how to read on.
pub fn thread_footer(t: &Thread, more: impl FnOnce(&str) -> String) -> String {
    let mut out = String::new();
    if t.omitted > 0 {
        let _ = writeln!(out, "({} jobs hidden: between other agents)", t.omitted);
    }
    if let Some(next) = &t.next {
        let _ = writeln!(out, "{}", more(next));
    }
    out
}

/// Each sender of `jobs` once, with the role the server gave it:
/// `linux main, windows worker`. Empty when no job carries a role.
pub fn sender_roles<'a>(jobs: impl Iterator<Item = &'a Job>) -> String {
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for j in jobs {
        if let Some(role) = &j.from_role
            && !seen.iter().any(|(name, r)| *name == j.from && *r == role)
        {
            seen.push((&j.from, role));
        }
    }
    seen.iter()
        .map(|(name, role)| format!("{name} {role}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(v: serde_json::Value) -> Job {
        serde_json::from_value(v).expect("job")
    }

    #[test]
    fn lines() {
        let m = job(serde_json::json!({
            "id": "j3", "from": "linux", "to": "windows", "kind": "message", "title": "Which branch?",
            "body": "Which branch?\r\nThe old one fails.\n", "state": "received", "seq": 1,
            "reply_to": "j2", "created_at": "2026-10-28T12:00:00Z",
        }));
        assert_eq!(
            line(&m),
            "j3 message linux -> windows [re j2] 2026-10-28 12:00: Which branch?\n    The old one fails."
        );
        let t = job(serde_json::json!({
            "id": "j4", "from": "linux", "from_role": "main", "to": "windows", "kind": "task",
            "title": "Run the probe", "body_preview": "Run it", "state": "done", "seq": 2,
            "created_at": "2026-10-28T12:05:09Z", "attachments": [{"name": "a", "size": 1, "sha256": "x"}],
        }));
        assert_eq!(
            line(&t),
            "j4 task done linux [main] -> windows 2026-10-28 12:05 (1 file): Run the probe"
        );
        let full = job(serde_json::json!({
            "id": "j5", "from": "windows", "to": "linux", "kind": "result", "title": "Done",
            "body": "Two devices.\nLog attached.", "state": "sent", "seq": 3,
        }));
        assert_eq!(
            line(&full),
            "j5 result sent windows -> linux: Done\n    Two devices.\n    Log attached."
        );
        let long = job(serde_json::json!({
            "id": "j6", "from": "windows", "to": "linux", "kind": "message", "title": "t",
            "body_preview": "start", "body_truncated": true, "body_size": 9000, "state": "sent", "seq": 4,
        }));
        assert_eq!(
            line(&long),
            "j6 message windows -> linux: start [truncated, 9 KiB in all]"
        );
        assert_eq!(sender_roles([&t, &full, &t].into_iter()), "linux main");
    }

    #[test]
    fn paths_encode_their_values() {
        assert_eq!(encode("50% ñ&x=1"), "50%25%20%C3%B1%26x%3D1");
        assert_eq!(
            thread_path("j3", Bodies::Full, Some(10), Some("1.2")).ok().as_deref(),
            Some("/threads/j3?full=1&limit=10&after=1.2")
        );
        assert_eq!(
            thread_path("j3", Bodies::Summary, None, None).ok().as_deref(),
            Some("/threads/j3")
        );
        assert!(thread_path("../x", Bodies::Summary, None, None).is_err());
        let path = search_path(&Filters {
            text: Some("gpu probe"),
            kind: Some("report"),
            since: Some("2026-10-28T12:00:00Z"),
            limit: Some(5),
            ..Filters::default()
        });
        assert_eq!(
            path,
            "/jobs?q=gpu%20probe&kind=report&since=2026-10-28T12%3A00%3A00Z&limit=5"
        );
        assert_eq!(search_path(&Filters::default()), "/jobs");
        assert_eq!(minute("2026-10-28T12:00:59Z"), "2026-10-28 12:00");
        assert_eq!(minute("odd"), "odd");
    }
}
