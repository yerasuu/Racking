//! How `rack` and `racking-mcp` show notices, messages and reports to the agent
//! that receives them: one line each, so a listing stays cheap to read.

use std::fmt::Write as _;

use crate::client::Job;

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

/// The line for a notice, message or report someone sent, or `None` for other
/// kinds. A notice or message shows its text after the colon, its further
/// lines indented by `CONTINUATION`; a report shows its title, size and files:
///
/// ```text
/// j7 notice[high] from linux: CI is red
/// j8 message from linux (reply expected, 2 files): Which branch?
///     The old one fails on Windows.
/// j9 report from windows: Probe log (12 KiB, 1 file)
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
    let mut line = format!("{} {}{} from {}", job.id, job.kind, priority_tag(job), job.from);
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
