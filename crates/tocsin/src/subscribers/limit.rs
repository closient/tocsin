// SPDX-License-Identifier: AGPL-3.0-only

//! Fixed-window counters for the sign-up rate limits.
//!
//! Each key gets `limit` tries per window, starting at its first try. The
//! counts live in memory: a restart forgets them, which costs at most one
//! window's worth of extra sign-ups.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// Keys held before expired windows are swept out.
const SWEEP_AT: usize = 10_000;

/// Counts tries per key.
pub struct Window<K> {
    limit: u32,
    length: Duration,
    counts: Mutex<HashMap<K, (Instant, u32)>>,
}

impl<K: Hash + Eq + Clone> Window<K> {
    /// `limit` tries per key per `length`.
    pub fn new(limit: u32, length: Duration) -> Self {
        Window {
            limit,
            length,
            counts: Mutex::new(HashMap::new()),
        }
    }

    /// Counts a try for `key`.
    ///
    /// # Errors
    ///
    /// The key is over its limit: how long until its window ends. The try
    /// is not counted.
    pub fn try_acquire(&self, key: &K) -> Result<(), Duration> {
        let now = Instant::now();
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if counts.len() >= SWEEP_AT {
            let length = self.length;
            counts.retain(|_, (start, _)| now.duration_since(*start) < length);
        }
        let entry = counts.entry(key.clone()).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.length {
            *entry = (now, 0);
        }
        if entry.1 >= self.limit {
            return Err(self.length.saturating_sub(now.duration_since(entry.0)));
        }
        entry.1 += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_key_gets_its_limit_per_window_and_learns_when_to_come_back() {
        let window = Window::new(2, Duration::from_secs(60));
        assert!(window.try_acquire(&"a").is_ok());
        assert!(window.try_acquire(&"a").is_ok());
        assert_eq!(window.try_acquire(&"a"), Err(Duration::from_secs(60)));
        assert!(window.try_acquire(&"b").is_ok(), "keys count apart");
        tokio::time::advance(Duration::from_secs(45)).await;
        assert_eq!(window.try_acquire(&"a"), Err(Duration::from_secs(15)));
        tokio::time::advance(Duration::from_secs(15)).await;
        assert!(window.try_acquire(&"a").is_ok(), "a new window starts");
    }
}
