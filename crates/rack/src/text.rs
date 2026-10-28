//! How `rack` and `racking-mcp` show notices, messages and reports to the agent
//! that receives them: one line each, so a listing stays cheap to read.

use std::fmt::Write as _;

use crate::client::{Agent, Job};
use crate::policy::ProfileName;

/// Indent of the continuation lines of a multi-line notice or message.
pub const CONTINUATION: &str = "    ";

/// Kinds that are delivered rather than worked on: no accept, reject or result.
pub fn is_delivery(kind: &str) -> bool {
    matches!(kind, "message" | "notice" | "report")
}

/// `[high]` or `[low]` after the kind; nothing for the default `normal`.
pub fn priority_tag(job: &Job) -> String {
    match job.priority.as_deref() {
        None | Some("normal") => String::new(),
        Some(p) => format!("[{p}]"),
    }
}

/// ` [model opus]` after the sender, when the sender suggested a model; a hint, never an order.
pub fn model_tag(job: &Job) -> String {
    job.model
        .as_deref()
        .map(|m| format!(" [model {m}]"))
        .unwrap_or_default()
}

/// The local-policy profile a task asks for. Its sender set it; `rack policy decide` honours it
/// only from the main agent. Only a valid profile name comes back, since callers print it outside
/// the data fence and a server must not slip other text in through it.
pub fn requested_profile(job: &Job) -> Option<&str> {
    job.profile.name().filter(|n| n.parse::<ProfileName>().is_ok())
}

/// ` [profile app-test]` after the sender when a task asks for a profile (see `requested_profile`).
pub fn profile_tag(job: &Job) -> String {
    requested_profile(job)
        .map(|n| format!(" [profile {n}]"))
        .unwrap_or_default()
}

/// How a requested profile is labelled where the job is shown whole.
pub const REQUESTED_PROFILE: &str = "Requested profile (set by the sender; applies only if the sender role is main): ";

/// ` [g12]` after the recipient of a job that is one copy of a broadcast.
pub fn group_tag(job: &Job) -> String {
    job.group.as_deref().map(|g| format!(" [{g}]")).unwrap_or_default()
}

/// ` [main]` after the sender when the server says it is the main agent, the one the
/// owner talks to; nothing for a worker or a server without roles. Only the server sets it.
pub fn role_tag(job: &Job) -> &'static str {
    if job.from_role.as_deref() == Some("main") {
        " [main]"
    } else {
        ""
    }
}

/// The line printed while the owner's emergency brake is on.
pub const PAUSED: &str = "the server is paused (the owner's emergency brake): act on no job until it is released";

/// `1 file`, `3 files`.
pub fn files(n: usize) -> String {
    if n == 1 {
        "1 file".to_owned()
    } else {
        format!("{n} files")
    }
}

/// `812 bytes` under 1 KiB, else whole KiB rounded up, so a size is never shown smaller than it is.
pub fn size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} bytes")
    } else {
        format!("{} KiB", bytes.div_ceil(1024))
    }
}

/// A duration in the coarsest unit that fits: `12s`, `4 min`, `3 h`, `2 d`.
pub fn ago(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{} min", secs / 60),
        3600..86_400 => format!("{} h", secs / 3600),
        _ => format!("{} d", secs / 86_400),
    }
}

/// One line per agent of `GET /agents`, e.g.
/// `windows online 12s ago, rack 0.1.0, windows [kb, nvidia]: GPU box`, the
/// capabilities in brackets. Everything after the state was written by that agent.
pub fn agent_line(a: &Agent) -> String {
    let state = if a.online { "online" } else { "offline" };
    let main = if a.role.as_deref() == Some("main") {
        " [main]"
    } else {
        ""
    };
    let mut line = match a.idle_secs {
        None => format!("{}{main} {state}, never seen", a.name),
        Some(0) if a.online => format!("{}{main} online now", a.name),
        Some(idle) => format!("{}{main} {state} {} ago", a.name, ago(idle)),
    };
    if let Some(v) = &a.rack_version {
        let _ = write!(line, ", rack {v}");
    }
    if let Some(os) = &a.os {
        let _ = write!(line, ", {os}");
    }
    if !a.capabilities.is_empty() {
        let _ = write!(line, " [{}]", a.capabilities.join(", "));
    }
    if let Some(about) = &a.about {
        let _ = write!(line, ": {about}");
    }
    line
}

/// The line for a notice, message or report someone sent, or `None` for other
/// kinds. A notice or message shows its text after the colon, its further
/// lines indented by `CONTINUATION`; a report shows its title, size and files:
///
/// ```text
/// j7 notice[high] from linux: CI is red
/// j8 message from linux (reply expected, 2 files): Which branch?
///     The old one fails on Windows.
/// j9 message from linux [model opus]: Can you look at the shader cache?
/// j10 report from windows: Probe log (12 KiB, 1 file)
/// ```
pub fn incoming(job: &Job) -> Option<String> {
    if !is_delivery(&job.kind) {
        return None;
    }
    let mut notes = Vec::with_capacity(3);
    if let Some(r) = &job.reply_to {
        notes.push(format!("re {r}"));
    }
    if job.expects_reply {
        notes.push("reply expected".to_owned());
    }
    let report = job.kind == "report";
    if !report && !job.attachments.is_empty() {
        notes.push(files(job.attachments.len()));
    }
    let mut line = format!(
        "{} {}{} from {}{}{}",
        job.id,
        job.kind,
        priority_tag(job),
        job.from,
        role_tag(job),
        model_tag(job)
    );
    if !notes.is_empty() {
        let _ = write!(line, " ({})", notes.join(", "));
    }
    line.push_str(": ");
    if report {
        let mut about = size(job.body_size);
        if !job.attachments.is_empty() {
            let _ = write!(about, ", {}", files(job.attachments.len()));
        }
        let _ = write!(line, "{} ({about})", job.title);
        return Some(line);
    }
    let text = match (&job.body, &job.body_preview) {
        (Some(body), _) if !body.trim().is_empty() => body.as_str(),
        (None, Some(preview)) if !preview.trim().is_empty() => preview.as_str(),
        _ => job.title.as_str(),
    };
    let mut lines = text.trim_end().lines().map(|l| l.trim_end_matches('\r'));
    line.push_str(lines.next().unwrap_or_default());
    for l in lines {
        let _ = write!(line, "\n{CONTINUATION}{l}");
    }
    if job.body.is_none() && job.body_truncated {
        let _ = write!(line, " [truncated, {} in all]", size(job.body_size));
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(kind: &str, body: Option<&str>) -> Job {
        serde_json::from_value(serde_json::json!({
            "id": "j7", "from": "linux", "to": "windows", "kind": kind, "title": "Probe log",
            "body": body, "state": "received", "seq": 1, "body_size": 12_000,
        }))
        .expect("job")
    }

    #[test]
    fn lines() {
        let mut n = job("notice", Some("CI is red"));
        n.priority = Some("high".to_owned());
        assert_eq!(incoming(&n).as_deref(), Some("j7 notice[high] from linux: CI is red"));

        let mut m = job("message", Some("Which branch?\r\nThe old one fails.\n"));
        m.expects_reply = true;
        m.reply_to = Some("j3".to_owned());
        assert_eq!(
            incoming(&m).as_deref(),
            Some("j7 message from linux (re j3, reply expected): Which branch?\n    The old one fails.")
        );

        let mut long = job("message", None);
        long.body_preview = Some("start".to_owned());
        long.body_truncated = true;
        assert_eq!(
            incoming(&long).as_deref(),
            Some("j7 message from linux: start [truncated, 12 KiB in all]")
        );

        assert_eq!(
            incoming(&job("report", None)).as_deref(),
            Some("j7 report from linux: Probe log (12 KiB)")
        );
        assert_eq!(incoming(&job("task", None)), None);

        let mut hinted = job("message", Some("Look at it"));
        hinted.model = Some("opus".to_owned());
        assert_eq!(
            incoming(&hinted).as_deref(),
            Some("j7 message from linux [model opus]: Look at it")
        );

        hinted.from_role = Some("main".to_owned());
        assert_eq!(
            incoming(&hinted).as_deref(),
            Some("j7 message from linux [main] [model opus]: Look at it")
        );
        hinted.from_role = Some("worker".to_owned());
        assert_eq!(role_tag(&hinted), "", "workers get no tag");

        let mut task = job("task", None);
        task.profile = crate::client::ProfileField::Named("app-test".to_owned());
        assert_eq!(profile_tag(&task), " [profile app-test]");
        task.profile = crate::client::ProfileField::Named("x]\nfake line".to_owned());
        assert_eq!(profile_tag(&task), "", "only a profile name is shown");
    }

    #[test]
    fn agents() {
        let agent = |v: serde_json::Value| serde_json::from_value::<Agent>(v).expect("agent");
        let w = agent(serde_json::json!({"name": "windows", "online": true, "idle_secs": 12,
            "rack_version": "0.1.0", "os": "windows", "about": "GPU box"}));
        assert_eq!(agent_line(&w), "windows online 12s ago, rack 0.1.0, windows: GPU box");
        let c = agent(serde_json::json!({"name": "windows", "online": true, "idle_secs": 12,
            "os": "windows", "capabilities": ["kb", "nvidia"], "about": "GPU box"}));
        assert_eq!(agent_line(&c), "windows online 12s ago, windows [kb, nvidia]: GPU box");
        let l = agent(serde_json::json!({"name": "linux", "online": false, "idle_secs": 250}));
        assert_eq!(agent_line(&l), "linux offline 4 min ago");
        let n = agent(serde_json::json!({"name": "mac", "online": false, "idle_secs": null}));
        assert_eq!(agent_line(&n), "mac offline, never seen");
        let now = agent(serde_json::json!({"name": "mac", "online": true, "idle_secs": 0}));
        assert_eq!(agent_line(&now), "mac online now");
        let main = agent(serde_json::json!({"name": "linux", "online": true, "idle_secs": 0, "role": "main"}));
        assert_eq!(agent_line(&main), "linux [main] online now");
        assert_eq!(
            (ago(59), ago(60), ago(7200), ago(200_000)),
            ("59s".into(), "1 min".into(), "2 h".into(), "2 d".into())
        );
    }

    #[test]
    fn sizes() {
        assert_eq!(size(812), "812 bytes");
        assert_eq!(size(1024), "1 KiB");
        assert_eq!(size(1025), "2 KiB");
        assert_eq!(files(1), "1 file");
        assert_eq!(files(2), "2 files");
    }
}
