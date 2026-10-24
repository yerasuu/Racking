//! `rack watch`: one short line per new job, for a Claude Code Monitor or an
//! `asyncRewake` hook (docs/agent-guide.md, "Waking an idle agent").
//!
//! It sleeps in the server's long wait, so it costs no requests while nothing
//! arrives. SIGINT and SIGTERM keep their default action (end the process): the
//! state file is replaced atomically and the OS drops the lock, so nothing is left
//! half written and no handler is needed. A closed stdout (the reader went away)
//! ends it on the next line it prints, since Rust ignores SIGPIPE and the write fails.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::client::{Client, Listing, RackError, Result, local, with_suffix};
use crate::state::State;

/// Longest line printed per job, `...` included: enough to know who wrote and about
/// what; the agent reads the rest with `rack show`.
pub const LINE_CHARS: usize = 200;
/// What the server waits at most per call (`limits.wait_max_secs`).
const WAIT_SECS: u64 = 55;
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// While the server stays unreachable, the failure is reported again this often,
/// so a long outage leaves a few lines, not one per retry.
const REPORT_EVERY: Duration = Duration::from_secs(5 * 60);

/// Whether to stop after the first batch of jobs (hooks) or run until killed (Monitor).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Once,
    Forever,
}

pub struct Watch<'a> {
    pub client: &'a Client,
    pub state_path: &'a Path,
    /// `--since N`: start there instead of the saved cursor.
    pub since: Option<i64>,
    pub mode: Mode,
    /// `--timeout SECS`: give up quietly (exit 0, nothing printed) after this long.
    pub deadline: Option<Instant>,
}

impl Watch<'_> {
    pub fn run(&self) -> Result<()> {
        // Held until the process ends; the OS releases it even on a kill.
        let Some(_lock) = self.lock()? else {
            return Ok(());
        };
        let mut since = match self.since {
            Some(n) => n,
            None => State::load(self.state_path)?.inbox_since,
        };
        let mut backoff = Backoff::new();
        let mut outage = Outage::default();
        let mut out = std::io::stdout().lock();
        loop {
            let Some(timeout) = self.wait_secs() else {
                return Ok(());
            };
            let listing = match self
                .client
                .get::<Listing>(&format!("/inbox/wait?since={since}&timeout={timeout}"))
            {
                Ok(l) => l,
                Err(e) if is_retryable(&e) => {
                    if outage.should_report(Instant::now()) {
                        eprintln!("rack: watch: {e}; retrying");
                    }
                    let delay = backoff.next_delay();
                    std::thread::sleep(
                        self.deadline
                            .map_or(delay, |d| delay.min(d.saturating_duration_since(Instant::now()))),
                    );
                    continue;
                }
                Err(e) => return Err(e),
            };
            backoff = Backoff::new();
            outage = Outage::default();
            for job in &listing.jobs {
                let line = short_line(&crate::incoming_line(job), LINE_CHARS);
                if let Err(e) = writeln!(out, "{line}").and_then(|()| out.flush()) {
                    // The cursor is not saved, so the next watch shows these jobs again.
                    return match e.kind() {
                        ErrorKind::BrokenPipe => Ok(()),
                        _ => Err(local("write to stdout", e)),
                    };
                }
            }
            if !listing.jobs.is_empty() {
                save_cursor(self.state_path, listing.next)?;
                if self.mode == Mode::Once {
                    return Ok(());
                }
            }
            since = listing.next;
            if self.since.is_none() {
                // `rack wait` or `rack ask` may have moved the shared cursor meanwhile.
                since = since.max(State::load(self.state_path)?.inbox_since);
            }
        }
    }

    /// Seconds for the next server wait, or `None` once the deadline has passed.
    fn wait_secs(&self) -> Option<u64> {
        match self.deadline {
            None => Some(WAIT_SECS),
            Some(d) => {
                let left = d.checked_duration_since(Instant::now())?;
                Some(left.as_secs().min(WAIT_SECS)).filter(|s| *s > 0)
            }
        }
    }

    /// One watch per state file, so hooks fired on every `Stop` do not pile up
    /// watchers that all print the same job. `None` when another one holds it.
    fn lock(&self) -> Result<Option<File>> {
        let path = with_suffix(self.state_path, ".watch.lock");
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| local(&format!("create {}", dir.display()), e))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| local(&format!("open {}", path.display()), e))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(TryLockError::WouldBlock) => {
                let note = format!(
                    "another rack watch already runs for {}; it reports the jobs",
                    self.state_path.display()
                );
                match self.mode {
                    // The hook that started this one is covered by the running watch: no error.
                    Mode::Once => {
                        eprintln!("rack: {note}");
                        Ok(None)
                    }
                    Mode::Forever => Err(RackError::Local(format!(
                        "{note}; stop it first, or use another --state"
                    ))),
                }
            }
            Err(TryLockError::Error(e)) => Err(local(&format!("lock {}", path.display()), e)),
        }
    }
}

/// Reloads the state before writing so links that `rack pull` saved meanwhile survive.
fn save_cursor(path: &Path, next: i64) -> Result<()> {
    let mut st = State::load(path)?;
    st.inbox_since = next;
    st.save(path)
}

/// Lost connections and server-side trouble pass; a bad token or request does not.
fn is_retryable(e: &RackError) -> bool {
    match e {
        RackError::Transport(_) => true,
        RackError::Api { status, .. } => *status == 429 || *status >= 500,
        RackError::Usage(_) | RackError::Local(_) | RackError::Unmet(_) => false,
    }
}

/// Retry delays: 1 s, doubling up to 30 s.
struct Backoff {
    next: Duration,
}

impl Backoff {
    fn new() -> Self {
        Self { next: BACKOFF_FIRST }
    }

    fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(BACKOFF_MAX);
        delay
    }
}

/// When the current run of failures was last reported.
#[derive(Default)]
struct Outage {
    reported: Option<Instant>,
}

impl Outage {
    fn should_report(&mut self, now: Instant) -> bool {
        match self.reported {
            Some(at) if now.duration_since(at) < REPORT_EVERY => false,
            _ => {
                self.reported = Some(now);
                true
            }
        }
    }
}

/// The first line of `text`, cut to `max` chars (on a char boundary) with `...`
/// when it was longer or had more lines.
pub fn short_line(text: &str, max: usize) -> String {
    let first = text.lines().next().unwrap_or_default();
    let more_lines = first.len() < text.trim_end().len();
    if !more_lines && first.chars().count() <= max {
        return first.to_owned();
    }
    let keep = max.saturating_sub(3);
    let cut = first.char_indices().nth(keep).map_or(first.len(), |(i, _)| i);
    format!("{}...", &first[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_thirty_seconds() {
        let mut b = Backoff::new();
        let secs: Vec<u64> = (0..8).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn outage_reports_first_then_every_five_minutes() {
        let mut o = Outage::default();
        let t = Instant::now();
        assert!(o.should_report(t));
        assert!(!o.should_report(t + Duration::from_secs(1)));
        assert!(!o.should_report(t + Duration::from_secs(299)));
        assert!(o.should_report(t + Duration::from_secs(300)));
        assert!(!o.should_report(t + Duration::from_secs(301)));
    }

    #[test]
    fn short_lines() {
        assert_eq!(
            short_line("j7 notice from linux: CI is red", 200),
            "j7 notice from linux: CI is red"
        );
        assert_eq!(
            short_line("j8 message from linux: a\n    b", 200),
            "j8 message from linux: a..."
        );
        let long = format!("j9 message from linux: {}", "é".repeat(300));
        let cut = short_line(&long, 200);
        assert_eq!(cut.chars().count(), 200);
        assert!(cut.ends_with("é..."), "cut between chars, never inside one");
        let exact = "x".repeat(200);
        assert_eq!(short_line(&exact, 200), exact);
    }

    #[test]
    fn cursor_is_saved_without_losing_links() {
        let dir = std::env::temp_dir().join(format!("rack-watch-{}", std::process::id()));
        let path = dir.join("state.json");
        let st = State {
            pull_since: 4,
            ..State::default()
        };
        st.save(&path).expect("save");
        save_cursor(&path, 9).expect("cursor");
        let back = State::load(&path).expect("load");
        assert_eq!((back.inbox_since, back.pull_since), (9, 4));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
