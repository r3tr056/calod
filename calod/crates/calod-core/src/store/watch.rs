//! Bounded version registry backing Redis `WATCH`-based optimistic transactions.
//!
//! Redis supports optimistic, transaction-style isolation via the
//! `WATCH` / `MULTI` / `EXEC` command sequence. A client `WATCH`es one or more
//! keys, queues commands, and on `EXEC` the transaction is aborted if any
//! watched key was modified after the `WATCH` was issued.
//!
//! [`WatchRegistry`] implements the version bookkeeping needed for this. It
//! associates each *currently watched* key with a monotonically-increasing
//! version number. The key invariant — and the reason memory stays bounded by
//! the number of watched keys rather than by the whole keyspace — is that a key
//! is only tracked while at least one client is watching it. Once the last
//! watcher leaves, the key (and its version) is forgotten.
//!
//! Typical flow:
//!
//! - On `WATCH key`, the client calls [`WatchRegistry::watch`] and records the
//!   returned version.
//! - On every write to a key, the store calls [`WatchRegistry::bump`]. This is a
//!   cheap no-op for keys nobody watches, so it is safe on the write hot path.
//! - On `EXEC`, the client compares each recorded version against
//!   [`WatchRegistry::current`]. If any differ, the transaction is aborted.
//! - On `UNWATCH` (or connection teardown / `EXEC` completion), the client calls
//!   [`WatchRegistry::unwatch`] once per previously watched key.
//!
//! Versions for a freshly-watched key start at `0`, and a key that is not
//! tracked also reports a current version of `0`. This means a client must not
//! assume version `0` is unique per `WATCH`; the comparison is only meaningful
//! while the key remains continuously watched, which is exactly the lifetime of
//! the registry entry.

use dashmap::DashMap;

/// Per-watched-key version tracker for optimistic transactions (WATCH/EXEC).
/// Only keys with >=1 active watcher are tracked; versions increase monotonically
/// and are forgotten once no client watches the key.
#[derive(Default)]
pub struct WatchRegistry {
    // key -> (version, watcher_count)
    entries: DashMap<String, (u64, usize)>,
}

impl WatchRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin watching `key` for one client. Returns the key's CURRENT version
    /// (newly-watched keys start at version 0). The caller records this value and
    /// later compares it via `current` at EXEC time.
    pub fn watch(&self, key: &str) -> u64 {
        let entry = self
            .entries
            .entry(key.to_string())
            .and_modify(|(_, n)| *n += 1)
            .or_insert((0, 1));
        entry.0
    }

    /// Stop watching `key` for one client. When the last watcher leaves, the key
    /// is dropped from the registry (its version is forgotten).
    pub fn unwatch(&self, key: &str) {
        // Decrement the watcher count, noting whether it reached zero, then drop
        // the `get_mut` guard before removing the entry to avoid re-entrant
        // locking on the same shard.
        let should_remove = if let Some(mut e) = self.entries.get_mut(key) {
            if e.1 > 0 {
                e.1 -= 1;
            }
            e.1 == 0
        } else {
            false
        };

        if should_remove {
            // Re-check the count under the removal lock so we don't drop an entry
            // that another thread re-watched between the guard release and here.
            self.entries.remove_if(key, |_, &(_, count)| count == 0);
        }
    }

    /// Record a write to `key`: if the key is currently watched, increment its
    /// version. No-op (cheap) if nothing watches the key — this is called on the
    /// write hot path for every mutated key.
    pub fn bump(&self, key: &str) {
        if let Some(mut e) = self.entries.get_mut(key) {
            e.0 += 1;
        }
    }

    /// The current version of `key` (0 if not tracked / never watched).
    pub fn current(&self, key: &str) -> u64 {
        self.entries.get(key).map(|e| e.0).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_returns_zero_for_fresh_key() {
        let reg = WatchRegistry::new();
        assert_eq!(reg.watch("k"), 0);
        // The current version of a freshly-watched key is also 0.
        assert_eq!(reg.current("k"), 0);
    }

    #[test]
    fn bump_only_affects_watched_keys() {
        let reg = WatchRegistry::new();

        // Bumping a key nobody watches does nothing; it remains at 0.
        reg.bump("unwatched");
        assert_eq!(reg.current("unwatched"), 0);

        // A watched key does get bumped.
        reg.watch("watched");
        reg.bump("watched");
        assert_eq!(reg.current("watched"), 1);

        // The unwatched key is still untracked and reads back as 0.
        assert_eq!(reg.current("unwatched"), 0);
    }

    #[test]
    fn watch_then_bump_changes_current_version() {
        let reg = WatchRegistry::new();

        let recorded = reg.watch("k");
        assert_eq!(recorded, 0);

        reg.bump("k");

        let now = reg.current("k");
        assert_eq!(now, 1);
        // The version observed at EXEC time differs from the one recorded at
        // WATCH time, so an optimistic transaction would correctly abort.
        assert_ne!(now, recorded);
    }

    #[test]
    fn two_watchers_keep_entry_until_last_leaves() {
        let reg = WatchRegistry::new();

        // Two clients watch the same key.
        reg.watch("k");
        reg.watch("k");

        // A write bumps the shared version.
        reg.bump("k");
        assert_eq!(reg.current("k"), 1);

        // One watcher leaves: the entry (and its version) survives.
        reg.unwatch("k");
        assert_eq!(reg.current("k"), 1);

        // The last watcher leaves: the key is forgotten and resets to 0.
        reg.unwatch("k");
        assert_eq!(reg.current("k"), 0);
    }

    #[test]
    fn bump_on_unwatched_key_is_noop() {
        let reg = WatchRegistry::new();

        // Never watched.
        reg.bump("ghost");
        assert_eq!(reg.current("ghost"), 0);

        // Watched then fully unwatched -> back to untracked, so bump is a no-op.
        reg.watch("ghost");
        reg.unwatch("ghost");
        reg.bump("ghost");
        assert_eq!(reg.current("ghost"), 0);
    }

    #[test]
    fn unwatch_unknown_key_does_not_panic() {
        let reg = WatchRegistry::new();
        // Defensive: unwatching a key that was never watched is harmless.
        reg.unwatch("never-seen");
        assert_eq!(reg.current("never-seen"), 0);
    }
}
