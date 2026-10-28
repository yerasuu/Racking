//! Who is online. Activity, and the `rack_version` and `os` that clients send as
//! headers on every request, are kept in memory per agent and written to SQLite
//! at most once every `FLUSH_SECS` per agent, so serving a request never costs a
//! write; a restart loses at most that much. `about` and `capabilities`
//! (`PUT /agents/me`) are rare and written at once.

use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use serde_json::{Value, json};

use crate::clock::{self, Clock};
use crate::error::ApiResult;
use crate::types::{AgentName, Label};

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
    /// What the agent says it has; empty when it published nothing.
    pub capabilities: Vec<String>,
}

/// A change to `Info`, per field: `None` keeps it, `Some(None)` clears it.
/// `capabilities` is replaced whole; an empty list clears it.
#[derive(Default)]
pub struct InfoUpdate {
    pub about: Option<Option<String>>,
    pub rack_version: Option<Option<String>>,
    pub os: Option<Option<String>>,
    pub capabilities: Option<Vec<String>>,
}

impl Info {
    fn updated(&self, change: InfoUpdate) -> Self {
        Self {
            about: change.about.unwrap_or_else(|| self.about.clone()),
            rack_version: change.rack_version.unwrap_or_else(|| self.rack_version.clone()),
            os: change.os.unwrap_or_else(|| self.os.clone()),
            capabilities: change.capabilities.unwrap_or_else(|| self.capabilities.clone()),
        }
    }
}

/// The client software a request named in `X-Rack-Version` and `X-Rack-Os`, already validated.
#[derive(Default)]
pub struct Software {
    pub rack_version: Option<Label>,
    pub os: Option<Label>,
}

/// What `touch` hands the caller to write to the store, when a write is due.
pub struct Flush {
    pub at: i64,
    pub rack_version: Option<String>,
    pub os: Option<String>,
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
    /// Read on every request and `GET /agents`; written only by `PUT /agents/me` and
    /// when a request names different software, so reads dominate.
    info: RwLock<Info>,
}

pub struct Presence {
    clock: Clock,
    /// Read on every request; written only when the admin API adds or removes an agent.
    slots: RwLock<Vec<Arc<Slot>>>,
}

impl Slot {
    fn new(name: AgentName, seen: i64, info: Info) -> Self {
        Self {
            name,
            last_seen: AtomicI64::new(seen),
            flushed: AtomicI64::new(seen),
            waiting: AtomicU32::new(0),
            info: RwLock::new(info),
        }
    }
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
            let info = kept.map(|k| k.info).unwrap_or_default();
            slots.push(Arc::new(Slot::new(name.clone(), seen, info)));
        }
        Self {
            clock,
            slots: RwLock::new(slots),
        }
    }

    fn slot(&self, agent: &AgentName) -> Option<Arc<Slot>> {
        let slots = self.slots.read().unwrap_or_else(PoisonError::into_inner);
        slots.iter().find(|s| &s.name == agent).map(Arc::clone)
    }

    /// Starts tracking an agent the admin API created; it has never been seen.
    pub fn add(&self, agent: &AgentName) {
        let mut slots = self.slots.write().unwrap_or_else(PoisonError::into_inner);
        if !slots.iter().any(|s| &s.name == agent) {
            slots.push(Arc::new(Slot::new(agent.clone(), NEVER, Info::default())));
        }
    }

    /// Stops listing a removed agent. A request already holding its slot finishes harmlessly.
    pub fn remove(&self, agent: &AgentName) {
        let mut slots = self.slots.write().unwrap_or_else(PoisonError::into_inner);
        slots.retain(|s| &s.name != agent);
    }

    /// Unix seconds of the agent's last request, or `None` if never seen.
    pub fn last_seen(&self, agent: &AgentName) -> Option<i64> {
        self.slot(agent)
            .map(|s| s.last_seen.load(Ordering::Relaxed))
            .filter(|t| *t != NEVER)
    }

    /// Records activity of `agent` now, and the software it named if that changed.
    /// Returns what to write to the store when its last write is `FLUSH_SECS`
    /// old; only one caller gets it.
    #[must_use]
    pub fn touch(&self, agent: &AgentName, software: Software) -> Option<Flush> {
        let slot = self.slot(agent)?;
        let now = self.clock.now();
        slot.last_seen.fetch_max(now, Ordering::Relaxed);
        let changed = |new: &Option<Label>, old: &Option<String>| {
            new.as_ref().is_some_and(|n| old.as_deref() != Some(n.as_str()))
        };
        let stale = {
            let info = slot.info.read().unwrap_or_else(PoisonError::into_inner);
            changed(&software.rack_version, &info.rack_version) || changed(&software.os, &info.os)
        };
        // The write lock is taken only when the software changed, so the usual request only reads.
        if stale {
            let mut info = slot.info.write().unwrap_or_else(PoisonError::into_inner);
            if let Some(v) = software.rack_version {
                info.rack_version = Some(v.into_inner());
            }
            if let Some(os) = software.os {
                info.os = Some(os.into_inner());
            }
        }
        let flushed = slot.flushed.load(Ordering::Relaxed);
        let due = now.saturating_sub(flushed) >= FLUSH_SECS
            && slot
                .flushed
                .compare_exchange(flushed, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok();
        if !due {
            return None;
        }
        let info = slot.info.read().unwrap_or_else(PoisonError::into_inner);
        Some(Flush {
            at: now,
            rack_version: info.rack_version.clone(),
            os: info.os.clone(),
        })
    }

    pub fn begin_wait(&self, agent: &AgentName) {
        if let Some(slot) = self.slot(agent) {
            slot.waiting.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Ends a wait started with `begin_wait`; the end counts as activity, as `touch`.
    #[must_use]
    pub fn end_wait(&self, agent: &AgentName) -> Option<Flush> {
        if let Some(slot) = self.slot(agent) {
            slot.waiting.fetch_sub(1, Ordering::Relaxed);
        }
        self.touch(agent, Software::default())
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
        let slots = self.slots.read().unwrap_or_else(PoisonError::into_inner);
        json!({"agents": slots.iter().map(|s| entry(s, now)).collect::<Vec<_>>()})
    }

    pub fn agent_json(&self, agent: &AgentName) -> Value {
        let now = self.clock.now();
        self.slot(agent).map_or(Value::Null, |s| entry(&s, now))
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
        "capabilities": info.capabilities,
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

    fn at(flush: Option<Flush>) -> Option<i64> {
        flush.map(|f| f.at)
    }

    fn software(version: &str, os: &str) -> Software {
        Software {
            rack_version: version.parse().ok(),
            os: os.parse().ok(),
        }
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

        assert_eq!(
            at(p.touch(win, Software::default())),
            Some(1_000_000),
            "first activity is written"
        );
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
        let _ = p.touch(win, Software::default());
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
        assert_eq!(at(p.touch(win, Software::default())), None, "written 10 s ago");
        clock.advance(49);
        assert_eq!(at(p.touch(win, software("0.2.0", "windows"))), None);
        assert_eq!(
            p.agent_json(win)["rack_version"],
            "0.2.0",
            "memory is current before the write"
        );
        clock.advance(1);
        let flush = p.touch(win, Software::default()).expect("due");
        assert_eq!(
            (flush.at, flush.rack_version.as_deref(), flush.os.as_deref()),
            (1_000_050, Some("0.2.0"), Some("windows"))
        );
        assert_eq!(at(p.touch(win, Software::default())), None);
    }

    #[test]
    fn requests_without_software_keep_it() {
        let names = names();
        let p = Presence::new(names.iter(), Vec::new(), Clock::manual(0));
        let linux = &names[0];
        let _ = p.touch(linux, software("0.1.0", "linux"));
        let _ = p.touch(linux, Software::default());
        let e = p.agent_json(linux);
        assert_eq!(
            (e["rack_version"].clone(), e["os"].clone()),
            (json!("0.1.0"), json!("linux"))
        );
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
        assert_eq!(e["capabilities"], json!([]), "none published yet");
        let caps = |list: &[&str]| InfoUpdate {
            capabilities: Some(list.iter().map(|c| (*c).to_owned()).collect()),
            ..InfoUpdate::default()
        };
        let e = p
            .update_info(linux, caps(&["kb", "nvidia"]), |_| Ok(()))
            .expect("update");
        assert_eq!(e["capabilities"], json!(["kb", "nvidia"]));
        let e = p.update_info(linux, InfoUpdate::default(), |_| Ok(())).expect("update");
        assert_eq!(e["capabilities"], json!(["kb", "nvidia"]), "left out is kept");
        let e = p.update_info(linux, caps(&[]), |_| Ok(())).expect("update");
        assert_eq!(e["capabilities"], json!([]), "an empty list clears");
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
