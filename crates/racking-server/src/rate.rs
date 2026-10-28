//! Jobs per sender per minute, counted in memory: a restart forgets at most one
//! minute of counts, which is fine for a guard against runaway agents.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{PoisonError, RwLock};

use crate::clock::Clock;
use crate::types::AgentName;

/// A fixed window per wall-clock minute and sender.
pub struct PerMinute {
    limit: u32,
    clock: Clock,
    /// Each window packs the minute in the high 32 bits and the count in the low
    /// 32, so one atomic update moves to a new minute and counts at once. The map
    /// is written only the first time a sender sends, so reads dominate.
    windows: RwLock<HashMap<AgentName, AtomicU64>>,
}

impl PerMinute {
    pub fn new(limit: u32, clock: Clock) -> Self {
        Self {
            limit,
            clock,
            windows: RwLock::new(HashMap::new()),
        }
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// Counts one job for `sender`. When its minute is full, returns the seconds
    /// until the next one and counts nothing.
    pub fn admit(&self, sender: &AgentName) -> Result<(), u64> {
        let now = self.clock.now();
        // Truncating the minute only matters when two windows 2^32 minutes apart meet.
        let minute = now.div_euclid(60) as u32;
        let take = |packed: u64| {
            let (m, count) = ((packed >> 32) as u32, packed as u32);
            if m != minute {
                Some(u64::from(minute) << 32 | 1)
            } else if count < self.limit {
                Some(packed + 1)
            } else {
                None
            }
        };
        let full = || now.rem_euclid(60).abs_diff(60);
        {
            let windows = self.windows.read().unwrap_or_else(PoisonError::into_inner);
            if let Some(w) = windows.get(sender) {
                return w
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, take)
                    .map(drop)
                    .map_err(|_| full());
            }
        }
        let mut windows = self.windows.write().unwrap_or_else(PoisonError::into_inner);
        let w = windows.entry(sender.clone()).or_insert_with(|| AtomicU64::new(0));
        w.fetch_update(Ordering::Relaxed, Ordering::Relaxed, take)
            .map(drop)
            .map_err(|_| full())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(n: &str) -> AgentName {
        n.parse().unwrap_or_else(|e: String| panic!("{e}"))
    }

    #[test]
    fn counts_per_sender_and_minute() {
        let clock = Clock::manual(600);
        let rate = PerMinute::new(2, clock.clone());
        let (a, b) = (name("a"), name("b"));
        assert_eq!(rate.admit(&a), Ok(()));
        assert_eq!(rate.admit(&a), Ok(()));
        clock.advance(15);
        assert_eq!(rate.admit(&a), Err(45), "full until the minute ends");
        assert_eq!(rate.admit(&b), Ok(()), "each sender has its own window");
        clock.advance(45);
        assert_eq!(rate.admit(&a), Ok(()), "a new minute starts over");
        assert_eq!(rate.admit(&a), Ok(()));
        assert_eq!(rate.admit(&a), Err(60));
    }
}
