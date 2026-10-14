//! Decoded looping videos (GIFs), by file and width, within a byte budget.
//! Every frame is kept uncompressed, so one long clip can take hundreds of
//! megabytes; counting loops wouldn't bound memory.
//!
//! Loops asked for within the last second are never evicted: they're on
//! screen, and evicting them would only decode them again on the next frame.
//! The budget can be exceeded while that many are visible.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub type LoopKey = (PathBuf, u32);

/// Decoded pixels kept at once (as `LruImageCache`).
const BUDGET_BYTES: usize = 256 * 1024 * 1024;
/// Also bounds entries that hold no pixels (failed decodes).
const MAX_ENTRIES: usize = 256;
/// Asked for this recently = still on screen.
const IN_USE: Duration = Duration::from_secs(1);

pub enum LoopState<T> {
    Loading,
    Ready(T),
    Failed,
}

struct Entry<T> {
    state: LoopState<T>,
    bytes: usize,
    last_used: Instant,
}

pub struct LoopCache<T> {
    entries: HashMap<LoopKey, Entry<T>>,
    bytes: usize,
    budget: usize,
}

impl<T> Default for LoopCache<T> {
    fn default() -> Self {
        Self { entries: HashMap::new(), bytes: 0, budget: BUDGET_BYTES }
    }
}

impl<T> LoopCache<T> {
    /// Looks up `key`, marking it in use. `None`: not cached; start a decode
    /// with [`Self::start`].
    pub fn get(&mut self, key: &LoopKey, now: Instant) -> Option<&LoopState<T>> {
        let entry = self.entries.get_mut(key)?;
        entry.last_used = now;
        Some(&entry.state)
    }

    pub fn start(&mut self, key: LoopKey, now: Instant) {
        self.entries.insert(key, Entry { state: LoopState::Loading, bytes: 0, last_used: now });
    }

    /// Stores a finished decode, unless the entry was evicted meanwhile.
    pub fn finish(&mut self, key: &LoopKey, state: LoopState<T>, bytes: usize) {
        if let Some(entry) = self.entries.get_mut(key) {
            self.bytes = self.bytes - entry.bytes + bytes;
            entry.state = state;
            entry.bytes = bytes;
        }
    }

    /// Evicts least recently used loops that aren't on screen or loading
    /// until within budget; returns their images to release.
    pub fn trim(&mut self, now: Instant) -> Vec<T> {
        let mut released = Vec::new();
        while self.bytes > self.budget || self.entries.len() > MAX_ENTRIES {
            let victim = self
                .entries
                .iter()
                .filter(|(_, e)| !matches!(e.state, LoopState::Loading) && now.duration_since(e.last_used) >= IN_USE)
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone());
            let Some(entry) = victim.and_then(|k| self.entries.remove(&k)) else { break };
            self.bytes -= entry.bytes;
            if let LoopState::Ready(image) = entry.state {
                released.push(image);
            }
        }
        released
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u32) -> LoopKey {
        (PathBuf::from(format!("{n}.mp4")), 100)
    }

    fn ready(cache: &mut LoopCache<u32>, n: u32, bytes: usize, at: Instant) {
        cache.start(key(n), at);
        cache.finish(&key(n), LoopState::Ready(n), bytes);
    }

    #[test]
    fn evicts_by_bytes_but_never_what_is_on_screen() {
        let t0 = Instant::now();
        let mut cache = LoopCache { budget: 100, ..Default::default() };
        ready(&mut cache, 1, 60, t0);
        ready(&mut cache, 2, 60, t0);
        // Both just drawn: over budget, but nothing is evicted.
        assert!(cache.trim(t0).is_empty());

        // Later, 2 is still drawn and 1 isn't: 1 goes.
        let t1 = t0 + Duration::from_secs(2);
        assert!(cache.get(&key(2), t1).is_some());
        assert_eq!(cache.trim(t1), vec![1]);
        assert!(cache.get(&key(1), t1).is_none());
        assert_eq!(cache.bytes, 60);
    }

    #[test]
    fn loading_entries_stay_and_late_results_are_dropped() {
        let t0 = Instant::now();
        let mut cache = LoopCache::<u32> { budget: 0, ..Default::default() };
        cache.start(key(1), t0);
        assert!(cache.trim(t0 + Duration::from_secs(5)).is_empty());
        cache.finish(&key(1), LoopState::Failed, 0);
        assert!(cache.trim(t0 + Duration::from_secs(5)).is_empty(), "failed entries cost nothing");
        // Finishing something no longer cached changes nothing.
        cache.finish(&key(9), LoopState::Ready(9), 50);
        assert_eq!(cache.bytes, 0);
    }
}
