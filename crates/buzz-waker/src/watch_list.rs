//! This daemon's own live known-agent set — `crate::effects`'s baseline for
//! `confirm_author_not_known_agent`, `PLANS/BUZZ_WAKER_DESIGN.md` §12 build
//! order step 3.
//!
//! Before the dynamic supervisor, this was a frozen `Arc<[String]>` snapshot
//! taken once at startup from `WAKER_AGENTS_CONFIG_PATH` — safe only because
//! the set never changed for the life of the process. Once agents can be
//! added or removed at runtime (a roster reissue), a frozen snapshot goes
//! stale: an agent added after startup would not be "known" to
//! `confirm_author_not_known_agent`, and a mention it authored could then
//! wake another agent — exactly the agent-to-agent wake loop that guard
//! exists to prevent (`crate::decide::select_wake_candidates`'s own doc).
//! [`WatchList`] fixes that by being read live rather than snapshotted:
//! every clone shares the same underlying set, so an `insert`/`remove` from
//! the supervisor is visible to every in-flight wake attempt's
//! `confirm_author_not_known_agent` check immediately.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use crate::decide::normalize_pubkey;

/// A cheaply-cloneable, live-shared set of normalized agent pubkeys.
#[derive(Debug, Clone, Default)]
pub struct WatchList {
    inner: Arc<Mutex<HashSet<String>>>,
}

impl WatchList {
    /// An empty watch list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Recover from a poisoned lock rather than propagating it — a panic in
    /// one reader must not permanently blind every future watch-list check.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Add `pubkey` to the set.
    pub fn insert(&self, pubkey: &str) {
        self.lock().insert(normalize_pubkey(pubkey));
    }

    /// Remove `pubkey` from the set. A no-op if it was never present.
    pub fn remove(&self, pubkey: &str) {
        self.lock().remove(&normalize_pubkey(pubkey));
    }

    /// Whether `pubkey` is currently in the set, comparison
    /// case/whitespace-insensitive (both sides normalized).
    #[must_use]
    pub fn contains(&self, pubkey: &str) -> bool {
        self.lock().contains(&normalize_pubkey(pubkey))
    }
}

impl From<Vec<String>> for WatchList {
    /// Build a watch list already populated with `pubkeys` — the shape
    /// existing tests already construct a frozen `Arc<[String]>` with
    /// (`Arc::from(vec![...])`), so callers only need to swap the type, not
    /// the construction pattern.
    fn from(pubkeys: Vec<String>) -> Self {
        let set = pubkeys.iter().map(|p| normalize_pubkey(p)).collect();
        Self {
            inner: Arc::new(Mutex::new(set)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_list_is_empty() {
        let list = WatchList::new();
        assert!(!list.contains(&"a".repeat(64)));
    }

    #[test]
    fn an_inserted_pubkey_is_found() {
        let list = WatchList::new();
        let pubkey = "a".repeat(64);
        list.insert(&pubkey);
        assert!(list.contains(&pubkey));
    }

    #[test]
    fn a_removed_pubkey_is_no_longer_found() {
        let list = WatchList::new();
        let pubkey = "a".repeat(64);
        list.insert(&pubkey);
        list.remove(&pubkey);
        assert!(!list.contains(&pubkey));
    }

    #[test]
    fn membership_is_case_and_whitespace_insensitive() {
        let list = WatchList::new();
        list.insert(" AA ");
        assert!(list.contains("aa"));
    }

    #[test]
    fn clones_share_the_same_underlying_set() {
        let list = WatchList::new();
        let clone = list.clone();
        let pubkey = "a".repeat(64);

        clone.insert(&pubkey);

        assert!(
            list.contains(&pubkey),
            "a clone must be a shared handle, not an independent copy"
        );
    }

    #[test]
    fn from_vec_normalizes_every_entry() {
        let list = WatchList::from(vec![" AA ".to_string()]);
        assert!(list.contains("aa"));
    }
}
