//! Ask and wait: block until the recipient of a message answers it, shared by
//! `rack ask` and `racking_ask`. Built only on `/outbox/wait` (the asked job
//! becomes `answered` in the same transaction that stores the reply) and
//! `/inbox` (which delivers the reply like any other inbox read).

use std::time::Instant;

use crate::client::{Agents, Client, Job, Listing, Result};
use crate::text;

/// The server's longest wait; longer asks loop.
const WAIT_MAX_SECS: u64 = 55;
const PAGE: u32 = 200;

/// The reply found in the inbox.
pub struct Found {
    pub reply: Job,
    /// Inbox jobs between the cursor and the reply, oldest first.
    pub earlier: Vec<Job>,
    /// Whether the reply was found past the given cursor, so `earlier` holds
    /// every job up to it and the cursor may move to `reply.seq`. False when it
    /// had already been read past the cursor and was found from the start.
    pub after_cursor: bool,
}

/// A warning line when `to` is offline by `GET /agents`, e.g. `windows has not
/// been seen for 4 min`. `None` when it is online, unknown, or the server has
/// no presence: the warning is advice, never a reason to fail.
pub fn absence(c: &Client, to: &str) -> Option<String> {
    let all: Agents = c.get("/agents").ok()?;
    let agent = all.agents.into_iter().find(|a| a.name == to)?;
    if agent.online {
        return None;
    }
    Some(match agent.idle_secs {
        Some(idle) => format!("{to} has not been seen for {}", text::ago(idle)),
        None => format!("{to} has not been seen since the server started keeping track"),
    })
}

/// Waits until `deadline` for the reply to `asked`, a job this agent sent with
/// `expects_reply`, looking for it in the inbox past `since`. `None` when no
/// reply came in time.
pub fn wait_reply(c: &Client, asked: &Job, since: i64, deadline: Instant) -> Result<Option<Found>> {
    if !wait_answered(c, asked, deadline)? {
        return Ok(None);
    }
    if let Some((reply, earlier)) = find_reply(c, asked, since)? {
        return Ok(Some(Found {
            reply,
            earlier,
            after_cursor: true,
        }));
    }
    if since == 0 {
        return Ok(None);
    }
    Ok(find_reply(c, asked, 0)?.map(|(reply, _)| Found {
        reply,
        earlier: Vec::new(),
        after_cursor: false,
    }))
}

/// Whether `asked` is `answered` by `deadline`, following its outbox changes from its own `updated_seq`.
fn wait_answered(c: &Client, asked: &Job, deadline: Instant) -> Result<bool> {
    if asked.state == "answered" {
        return Ok(true);
    }
    let mut cursor = asked.updated_seq;
    loop {
        // Rounded up, so a deadline less than a second away still gets a real wait.
        let left = deadline.saturating_duration_since(Instant::now());
        let secs = (left.as_secs() + u64::from(left.subsec_nanos() > 0)).min(WAIT_MAX_SECS);
        let l: Listing = c.get(&format!("/outbox/wait?since={cursor}&timeout={secs}&limit={PAGE}"))?;
        if l.jobs.iter().any(|j| j.id == asked.id && j.state == "answered") {
            return Ok(true);
        }
        cursor = l.next;
        // A zero-second wait was the last look; anything longer may have ended early on another job's change.
        if secs == 0 || Instant::now() >= deadline {
            return Ok(false);
        }
    }
}

/// The reply to `asked` in the inbox past `since`, with the jobs before it.
fn find_reply(c: &Client, asked: &Job, since: i64) -> Result<Option<(Job, Vec<Job>)>> {
    let mut earlier = Vec::new();
    let mut cursor = since;
    loop {
        let l: Listing = c.get(&format!("/inbox?since={cursor}&limit={PAGE}"))?;
        if l.jobs.is_empty() {
            return Ok(None);
        }
        for j in l.jobs {
            if j.reply_to.as_deref() == Some(asked.id.as_str()) && j.from == asked.to {
                return Ok(Some((j, earlier)));
            }
            earlier.push(j);
        }
        cursor = l.next;
    }
}
