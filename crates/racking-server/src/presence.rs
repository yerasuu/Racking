//! Who is online. Activity is kept in memory per agent and written to SQLite at
//! most once every `FLUSH_SECS` per agent, so serving a request never costs a
//! write; a restart loses at most that much of `last_seen`. What agents publish
//! about themselves (`PUT /agents/me`) is rare and written at once.

use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::{PoisonError, RwLock};

use serde_json::{Value, json};

use crate::clock::{self, Clock};
use crate::error::ApiResult;
use crate::types::AgentName;

/// An agent is online when its last request is at most this old, or while it is inside a wait.
pub const ONLINE_SECS: i64 = 90;
const FLUSH_SECS: i64 = 60;
const NEVER: i64 = i64::MIN;

/// What an agent published about itself.
#[derive(Default, Clone)]
pub struct Info {
    pub about: Option<String>,
    pub rack_version: Option<String>,
    pub os: Option<String>,
}

/// A change to `Info`, per field: `None` keeps it, `Some(None)` clears it.
#[derive(Default)]
pub struct InfoUpdate {
    pub about: Option<Option<String>>,
    pub rack_version: Option<Option<String>>,
    pub os: Option<Option<String>>,
}

impl Info {
    fn updated(&self, change: InfoUpdate) -> Self {
        Self {
            about: change.about.unwrap_or_else(|| self.about.clone()),
            rack_version: change.rack_version.unwrap_or_else(|| self.rack_version.clone()),
            os: change.os.unwrap_or_else(|| self.os.clone()),
        }
    }
}

/// An agent's row as the store kept it across a restart.
pub struct Stored {
    pub name: String,
    pub last_seen: Option<i64>,
    pub info: Info,
}

struct Slot {
    name: AgentName,
    last_seen: AtomicI64,
    /// The `last_seen` last written to the store, to throttle writes.
    flushed: AtomicI64,
    /// Waits in progress: counted, not flagged, since one agent may run several at once.
    waiting: AtomicU32,
    /// Read on every `GET /agents`, written only by `PUT /agents/me`.
    info: RwLock<Info>,
}

pub struct Presence {
    clock: Clock,
    slots: Vec<Slot>,
}

impl Presence {
    /// One slot per configured agent; stored rows of agents no longer configured are ignored.
    pub fn new<'a>(names: impl ExactSizeIterator<Item = &'a AgentName>, mut stored: Vec<Stored>, clock: Clock) -> Self {
        let mut slots = Vec::with_capacity(names.len());
        for name in names {
            let kept = stored
                .iter()
                .position(|s| s.name == name.as_str())
                .map(|i| stored.swap_remove(i));
            let seen = kept.as_ref().and_then(|k| k.last_seen).unwrap_or(NEVER);
            slots.push(Slot {
                name: name.clone(),
                last_seen: AtomicI64::new(seen),
                flushed: AtomicI64::new(seen),
                waiting: AtomicU32::new(0),
                info: RwLock::new(kept.map(|k| k.info).unwrap_or_default()),
            });
        }
        Self { clock, slots }
    }

    fn slot(&self, agent: &AgentName) -> Option<&Slot> {
        self.slots.iter().find(|s| &s.name == agent)
    }

    /// Records activity of `agent` now. Returns the time to write to the store
    /// when its last write is `FLUSH_SECS` old; only one caller gets it.
    #[must_use]
    pub fn touch(&self, agent: &AgentName) -> Option<i64> {
        let slot = self.slot(agent)?;
        let now = self.clock.now();
        slot.last_seen.fetch_max(now, Ordering::Relaxed);
        let flushed = slot.flushed.load(Ordering::Relaxed);
        let due = now.saturating_sub(flushed) >= FLUSH_SECS
            && slot
                .flushed
                .compare_exchange(flushed, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok();
        due.then_some(now)
    }

    pub fn begin_wait(&self, agent: &AgentName) {
        if let Some(slot) = self.slot(agent) {
            slot.waiting.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Ends a wait started with `begin_wait`; the end counts as activity, as `touch`.
    #[must_use]
    pub fn end_wait(&self, agent: &AgentName) -> Option<i64> {
        if let Some(slot) = self.slot(agent) {
            slot.waiting.fetch_sub(1, Ordering::Relaxed);
        }
        self.touch(agent)
    }

    /// Applies `change` to the agent's info once `save` stored the result. The
    /// write lock is held across `save` so two updates reach the store in the
    /// same order as memory.
    pub fn update_info(
        &self,
        agent: &AgentName,
        change: InfoUpdate,
        save: impl FnOnce(&Info) -> ApiResult<()>,
    ) -> ApiResult<Value> {
        if let Some(slot) = self.slot(agent) {
            let mut info = slot.info.write().unwrap_or_else(PoisonError::into_inner);
            let next = info.updated(change);
            save(&next)?;
            *info = next;
        }
        Ok(self.agent_json(agent))
    }

    /// `{"agents": [...]}`, every configured agent in config order. Carries no token or admin flag.
    pub fn to_json(&self) -> Value {
        let now = self.clock.now();
        json!({"agents": self.slots.iter().map(|s| entry(s, now)).collect::<Vec<_>>()})
    }

    pub fn agent_json(&self, agent: &AgentName) -> Value {
        let now = self.clock.now();
        self.slot(agent).map_or(Value::Null, |s| entry(s, now))
    }
}

fn entry(slot: &Slot, now: i64) -> Value {
    let waiting = slot.waiting.load(Ordering::Relaxed) > 0;
    let seen = slot.last_seen.load(Ordering::Relaxed);
    // An agent blocked in a wait is connected right now, so it is seen now.
    let last_seen = if waiting {
        Some(now)
    } else {
        (seen != NEVER).then_some(seen)
    };
    let idle = last_seen.map(|t| now.saturating_sub(t).max(0));
    let info = slot.info.read().unwrap_or_else(PoisonError::into_inner);
    json!({
        "name": slot.name.as_str(),
        "online": waiting || idle.is_some_and(|i| i <= ONLINE_SECS),
        "last_seen": last_seen.map(clock::rfc3339),
        "idle_secs": idle,
        "rack_version": info.rack_version,
        "os": info.os,
        "about": info.about,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<AgentName> {
        ["linux", "windows"]
            .iter()
            .map(|n| n.parse().unwrap_or_else(|e: String| panic!("{e}")))
            .collect()
    }

    fn online(p: &Presence, agent: &AgentName) -> (bool, Value) {
        let e = p.agent_json(agent);
        (e["online"] == true, e["idle_secs"].clone())
    }

    #[test]
    fn online_rule_follows_the_clock() {
        let clock = Clock::manual(1_000_000);
        let names = names();
        let p = Presence::new(names.iter(), Vec::new(), clock.clone());
        let win = &names[1];
        assert_eq!(online(&p, win), (false, Value::Null), "never seen");
        assert_eq!(p.agent_json(win)["last_seen"], Value::Null);

        assert_eq!(p.touch(win), Some(1_000_000), "first activity is written");
        clock.advance(ONLINE_SECS);
        assert_eq!(online(&p, win), (true, json!(ONLINE_SECS)));
        clock.advance(1);
        assert_eq!(online(&p, win), (false, json!(ONLINE_SECS + 1)));
        assert_eq!(p.agent_json(win)["last_seen"], "1970-01-12T13:46:40Z");
    }

    #[test]
    fn a_wait_keeps_an_agent_online() {
        let clock = Clock::manual(1_000_000);
        let names = names();
        let p = Presence::new(names.iter(), Vec::new(), clock.clone());
        let win = &names[1];
        let _ = p.touch(win);
        p.begin_wait(win);
        p.begin_wait(win);
        clock.advance(600);
        assert_eq!(online(&p, win), (true, json!(0)));
        let _ = p.end_wait(win);
        assert_eq!(online(&p, win), (true, json!(0)), "one wait still runs");
        let _ = p.end_wait(win);
        clock.advance(ONLINE_SECS + 1);
        assert_eq!(
            online(&p, win),
            (false, json!(ONLINE_SECS + 1)),
            "last_seen is the end of the wait"
        );
    }

    #[test]
    fn writes_are_throttled() {
        let clock = Clock::manual(1_000_000);
        let names = names();
        let stored = vec![Stored {
            name: "windows".to_owned(),
            last_seen: Some(999_990),
            info: Info {
                about: Some("kept".to_owned()),
                ..Info::default()
            },
        }];
        let p = Presence::new(names.iter(), stored, clock.clone());
        let win = &names[1];
        assert_eq!(p.agent_json(win)["about"], "kept");
        assert_eq!(p.touch(win), None, "written 10 s ago");
        clock.advance(49);
        assert_eq!(p.touch(win), None);
        clock.advance(1);
        assert_eq!(p.touch(win), Some(1_000_050));
        assert_eq!(p.touch(win), None);
    }

    #[test]
    fn info_updates_keep_or_clear_fields() {
        let names = names();
        let p = Presence::new(names.iter(), Vec::new(), Clock::manual(0));
        let linux = &names[0];
        let set = InfoUpdate {
            about: Some(Some("on the desk".to_owned())),
            os: Some(Some("linux".to_owned())),
            ..InfoUpdate::default()
        };
        let e = p.update_info(linux, set, |_| Ok(())).expect("update");
        assert_eq!(
            (e["about"].clone(), e["os"].clone()),
            (json!("on the desk"), json!("linux"))
        );
        let clear = InfoUpdate {
            about: Some(None),
            ..InfoUpdate::default()
        };
        let e = p.update_info(linux, clear, |_| Ok(())).expect("update");
        assert_eq!((e["about"].clone(), e["os"].clone()), (Value::Null, json!("linux")));
        let failed = InfoUpdate {
            os: Some(None),
            ..InfoUpdate::default()
        };
        let err = p.update_info(linux, failed, |_| {
            Err(crate::error::ApiError::bad_request("store down"))
        });
        assert!(err.is_err());
        assert_eq!(p.agent_json(linux)["os"], "linux", "memory follows the store");
    }
}
