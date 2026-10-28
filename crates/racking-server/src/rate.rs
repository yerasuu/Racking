//! Counts per key in fixed wall-clock windows, in memory: jobs per sender per
//! minute, and failed pairing codes per peer address. A restart forgets at most
//! one window of counts, which is fine for guards against runaway or guessing peers.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{PoisonError, RwLock};

use crate::clock::Clock;
use crate::types::AgentName;

/// Jobs per sender per minute.
pub type PerMinute = PerWindow<AgentName>;

/// A fixed window of `window` seconds per key.
pub struct PerWindow<K> {
    limit: u32,
    window: i64,
    clock: Clock,
    /// Each window packs its number in the high 32 bits and the count in the low
    /// 32, so one atomic update moves to a new window and counts at once. The map
    /// is written only the first time a key counts, so reads dominate.
    windows: RwLock<HashMap<K, AtomicU64>>,
}

impl PerWindow<AgentName> {
    pub fn new(limit: u32, clock: Clock) -> Self {
        Self::with_window(limit, 60, clock)
    }
}

impl<K: Hash + Eq + Clone> PerWindow<K> {
    pub fn with_window(limit: u32, window: i64, clock: Clock) -> Self {
        Self {
            limit,
            window,
            clock,
            windows: RwLock::new(HashMap::new()),
        }
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// Counts one for `key`. When its window is full, returns the seconds until
    /// the next one and counts nothing.
    pub fn admit(&self, key: &K) -> Result<(), u64> {
        self.admit_many(key, 1)
    }

    /// Counts `n` for `key` at once, or nothing when fewer than `n` are left in its window,
    /// so a broadcast is admitted whole or not at all. Returns the seconds until the next window.
    pub fn admit_many(&self, key: &K, n: u32) -> Result<(), u64> {
        let now = self.clock.now();
        // Truncating the window number only matters when two windows 2^32 windows apart meet.
        let current = now.div_euclid(self.window) as u32;
        let take = |packed: u64| {
            let (w, count) = ((packed >> 32) as u32, packed as u32);
            if w != current {
                (n <= self.limit).then(|| u64::from(current) << 32 | u64::from(n))
            } else if count.saturating_add(n) <= self.limit {
                Some(packed + u64::from(n))
            } else {
                None
            }
        };
        let full = || now.rem_euclid(self.window).abs_diff(self.window);
        {
            let windows = self.windows.read().unwrap_or_else(PoisonError::into_inner);
            if let Some(w) = windows.get(key) {
                return w
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, take)
                    .map(drop)
                    .map_err(|_| full());
            }
        }
        let mut windows = self.windows.write().unwrap_or_else(PoisonError::into_inner);
        let w = windows.entry(key.clone()).or_insert_with(|| AtomicU64::new(0));
        w.fetch_update(Ordering::Relaxed, Ordering::Relaxed, take)
            .map(drop)
            .map_err(|_| full())
    }

    /// The seconds until `key`'s window ends while it is full, without counting.
    pub fn blocked(&self, key: &K) -> Option<u64> {
        let now = self.clock.now();
        let current = now.div_euclid(self.window) as u32;
        let windows = self.windows.read().unwrap_or_else(PoisonError::into_inner);
        let packed = windows.get(key)?.load(Ordering::Relaxed);
        let (w, count) = ((packed >> 32) as u32, packed as u32);
        (w == current && count >= self.limit).then(|| now.rem_euclid(self.window).abs_diff(self.window))
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

    #[test]
    fn many_at_once_count_whole_or_not_at_all() {
        let clock = Clock::manual(600);
        let rate = PerMinute::new(5, clock.clone());
        let a = name("a");
        assert_eq!(rate.admit_many(&a, 3), Ok(()));
        assert_eq!(rate.admit_many(&a, 3), Err(60), "two left, three asked: nothing counts");
        assert_eq!(rate.admit_many(&a, 2), Ok(()));
        assert_eq!(rate.admit(&a), Err(60));
        clock.advance(60);
        assert_eq!(rate.admit_many(&a, 6), Err(60), "more than a whole window never fits");
        assert_eq!(rate.admit_many(&a, 5), Ok(()));
    }

    #[test]
    fn longer_windows_block_until_they_end() {
        let clock = Clock::manual(900);
        let peers = PerWindow::with_window(2, 900, clock.clone());
        let ip: std::net::IpAddr = [192, 168, 1, 9].into();
        assert_eq!(peers.blocked(&ip), None);
        assert_eq!(peers.admit(&ip), Ok(()));
        assert_eq!(peers.blocked(&ip), None);
        assert_eq!(peers.admit(&ip), Ok(()));
        clock.advance(100);
        assert_eq!(peers.blocked(&ip), Some(800));
        clock.advance(800);
        assert_eq!(peers.blocked(&ip), None, "a new window starts over");
    }
}
