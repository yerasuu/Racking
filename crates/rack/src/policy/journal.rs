//! The two files the policy keeps next to it: the active profile, written only by
//! `rack policy decide --accept` and read by the hook, and the audit log of every decision.
//! Both are private to the owner (mode 600 on Unix).

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The audit log is rotated once it reaches this size; three old files are kept.
pub const AUDIT_MAX_BYTES: u64 = 1 << 20;
pub const AUDIT_KEEP: u32 = 3;
/// Reasons are cut to this length in the log so a long one cannot carry a job's content.
const REASON_MAX: usize = 300;

pub fn active_path(dir: &Path) -> PathBuf {
    dir.join("policy-active.json")
}

pub fn audit_path(dir: &Path) -> PathBuf {
    dir.join("policy-audit.log")
}

#[must_use]
pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The profile a worker is working in, for one accepted task, until `expires_at` (Unix seconds).
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct Active {
    pub job: String,
    pub profile: String,
    pub from: String,
    pub accepted_at: u64,
    pub expires_at: u64,
}

/// The active profile at `now`, if any. An expired one counts as none.
///
/// # Errors
///
/// A file that exists but cannot be read or parsed: the caller denies, since it cannot tell what
/// the owner's last decision was.
pub fn read_active(path: &Path, now: u64) -> Result<Option<Active>, String> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let active: Active =
        serde_json::from_str(&text).map_err(|e| format!("{} is damaged ({e}); delete it", path.display()))?;
    Ok((active.expires_at > now).then_some(active))
}

/// Replaces the active profile atomically, so the hook never reads half a file.
///
/// # Errors
///
/// The file could not be written.
pub fn write_active(path: &Path, active: &Active) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string(active).map_err(|e| e.to_string())?;
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut f = private(OpenOptions::new().write(true).create(true).truncate(true)).open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    };
    write().map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Ends the active profile; nothing to do when there is none.
///
/// # Errors
///
/// The file exists and could not be removed.
pub fn clear_active(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
    }
}

fn private(opts: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600)
    }
    #[cfg(not(unix))]
    {
        opts
    }
}

/// One line of the audit log. Never a job body, a command line or a token.
pub struct Entry<'a> {
    pub source: &'a str,
    pub decision: &'a str,
    pub job: Option<&'a str>,
    pub profile: Option<&'a str>,
    pub from: Option<&'a str>,
    pub from_role: Option<&'a str>,
    pub tool: Option<&'a str>,
    pub reason: &'a str,
}

impl Entry<'_> {
    fn json(&self, at: u64) -> Value {
        let reason: String = self.reason.chars().take(REASON_MAX).collect();
        json!({
            "at": at,
            "source": self.source,
            "decision": self.decision,
            "job": self.job,
            "profile": self.profile,
            "from": self.from,
            "from_role": self.from_role,
            "tool": self.tool,
            "reason": reason,
        })
    }
}

/// Appends `entry` to the log at `path`, rotating it first once it reached `AUDIT_MAX_BYTES`.
///
/// # Errors
///
/// The log could not be written; callers turn an allow into a deny then, so nothing is allowed
/// without a trace.
pub fn audit(path: &Path, entry: &Entry) -> Result<(), String> {
    let line = format!("{}\n", entry.json(now()));
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        if fs::metadata(path).is_ok_and(|m| m.len() >= AUDIT_MAX_BYTES) {
            rotate(path)?;
        }
        let mut f: File = private(OpenOptions::new().append(true).create(true)).open(path)?;
        f.write_all(line.as_bytes())
    };
    write().map_err(|e| format!("cannot write the audit log {}: {e}", path.display()))
}

fn rotated(path: &Path, n: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

fn rotate(path: &Path) -> std::io::Result<()> {
    for n in (1..AUDIT_KEEP).rev() {
        let from = rotated(path, n);
        if from.exists() {
            fs::rename(&from, rotated(path, n + 1))?;
        }
    }
    fs::rename(path, rotated(path, 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::tests::Fixture;

    fn entry(reason: &str) -> Entry<'_> {
        Entry {
            source: "decide",
            decision: "deny",
            job: Some("j1"),
            profile: None,
            from: Some("linux"),
            from_role: Some("worker"),
            tool: None,
            reason,
        }
    }

    #[test]
    fn audit_lines_are_json_and_rotate_keeping_three() {
        let f = Fixture::new("audit");
        let log = audit_path(&f.conf);
        audit(&log, &entry("first")).expect("audit");
        let line: Value = serde_json::from_str(fs::read_to_string(&log).expect("read").trim()).expect("json");
        assert_eq!(line["decision"], "deny");
        assert_eq!(line["from_role"], "worker");
        for round in 0..5 {
            fs::write(&log, vec![b'x'; AUDIT_MAX_BYTES as usize]).expect("fill");
            audit(&log, &entry(&format!("round {round}"))).expect("audit");
        }
        assert!(fs::metadata(&log).expect("log").len() < 1000);
        for n in 1..=AUDIT_KEEP {
            assert!(rotated(&log, n).exists(), "{n}");
        }
        assert!(!rotated(&log, AUDIT_KEEP + 1).exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&log).expect("log").permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn long_reasons_are_cut() {
        let e = entry(&"x".repeat(5000)).json(1);
        assert_eq!(e["reason"].as_str().expect("reason").len(), REASON_MAX);
    }

    #[test]
    fn active_expires_and_damage_is_an_error() {
        let f = Fixture::new("active");
        let path = active_path(&f.conf);
        assert_eq!(read_active(&path, 10).expect("none"), None);
        let a = Active {
            job: "j1".to_owned(),
            profile: "workdir".to_owned(),
            from: "linux".to_owned(),
            accepted_at: 10,
            expires_at: 20,
        };
        write_active(&path, &a).expect("write");
        assert_eq!(read_active(&path, 15).expect("read"), Some(a));
        assert_eq!(read_active(&path, 20).expect("expired"), None);
        fs::write(&path, "{").expect("damage");
        assert!(read_active(&path, 15).is_err());
        clear_active(&path).expect("clear");
        clear_active(&path).expect("clear twice");
    }
}
