//! Durable resume state: where to restart the feed after a reconnect — **G4**.
//!
//! Only live events trigger a wake, so a waker that restarts must re-ask the
//! relay for what it missed. The relay does serve history — a REQ delivers
//! stored events before EOSE — so the gap is recoverable, but only if the
//! resume point is chosen carefully.
//!
//! # Why the checkpoint is a low-water mark
//!
//! The obvious design — advance the cursor to the newest event that finished
//! processing, and lean on [`crate::RECONNECT_OVERLAP_SECS`] to re-cover
//! anything missed — **has a hole**, and it is worth spelling out because the
//! overlap looks like it should absorb this:
//!
//! 1. event `A` is accepted backdated by the full drift, `created_at = T-900`;
//! 2. `A` begins processing (a wake attempt can legitimately run for minutes:
//!    evidence window, teardown fence, then a deploy invocation);
//! 3. meanwhile event `B` is accepted future-dated by the full drift and
//!    completes, so a max-completed cursor jumps to roughly `T+1600`;
//! 4. the process dies with `A` still in flight.
//!
//! On restart `since = T+1600 - 1800 = T-200`, and `A` at `T-900` is **behind
//! the window**. It is never re-delivered and the mention is lost silently.
//!
//! Widening the overlap to cover it would mean sizing one constant against the
//! *duration of processing* — a numeric coupling with nothing enforcing it,
//! the same shape of mistake as pinning the overlap against the replay-floor
//! bound. So the checkpoint is instead the **minimum `created_at` across
//! everything still in flight**, falling back to the high-water mark when
//! nothing is. It cannot advance past unfinished work by construction, at any
//! processing duration and any drift.
//!
//! # Ordering
//!
//! The lowered checkpoint is persisted **before** the work starts
//! ([`CursorStore::admit`]) and raised only after a terminal outcome
//! ([`CursorStore::complete`]). Committing completion first would let a crash
//! mid-attempt replay the event and then dedupe it away unprocessed.
//! Processing-first is safe because provider deploy is idempotent — a
//! duplicate is a strict no-op.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::fence::{atomic_write, lock_path_for, FenceGuard};

/// Default bound on the completed-id ring.
///
/// Sized against a delivery burst, not against the desktop's
/// `SEEN_WAKE_EVENT_LIMIT` of 500 — that guards re-delivery on a live tap,
/// while this has to cover everything a reconnect can replay inside the
/// overlap window.
pub const DEFAULT_COMPLETED_RING: usize = 4096;

/// Failures reading or advancing the durable cursor.
#[derive(Debug, thiserror::Error)]
pub enum CursorError {
    /// The cursor file exists but could not be read or parsed.
    #[error("cursor state at {path} is unreadable ({reason}); refusing to guess a resume point")]
    Corrupt {
        /// The path that could not be read.
        path: String,
        /// Why.
        reason: String,
    },

    /// The cursor could not be made durable, or the fence could not be taken.
    #[error("could not persist cursor state to {path}: {reason}")]
    Persist {
        /// The path being written.
        path: String,
        /// Why.
        reason: String,
    },

    /// `complete` was called for an event that was never admitted.
    #[error("event {event_id} was completed without being admitted")]
    NotInFlight {
        /// The offending event id.
        event_id: String,
    },
}

/// Where to resume, or why we cannot resume usefully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resume {
    /// Re-subscribe with this `since`, in unix seconds.
    Since(u64),
    /// The gap has outgrown what a woken agent could still be shown.
    ///
    /// Past [`buzz_core::relay::REPLAY_FLOOR_MAX_AGE_SECS`] the harness clamps
    /// the replay floor, so an agent woken for one of these mentions would
    /// start up and never see it. Waking anyway would look like success and
    /// answer nobody, so this is surfaced as an operational failure instead.
    /// The caller should still resume from `since` — the events are worth
    /// processing — but must alert rather than report healthy.
    GapTooOld {
        /// The resume point, still usable for the subscription.
        since: u64,
        /// How far behind the checkpoint is, in seconds.
        behind_secs: u64,
        /// The bound it exceeded.
        max_age_secs: u64,
    },
}

/// Whether an event still needs processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Not seen before; now recorded in flight.
    Fresh,
    /// Already completed, or already in flight. Skip it.
    Duplicate,
}

/// The persisted resume state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// Resume point: the low-water mark described in the module docs.
    pub checkpoint_secs: u64,
    /// Newest `created_at` that has reached a terminal outcome.
    pub high_water_secs: u64,
    /// Bounded ring of completed event ids, oldest first.
    pub completed: VecDeque<String>,
}

/// File-backed [`Cursor`] with the same fence discipline as the floor store.
#[derive(Debug)]
pub struct CursorStore {
    path: PathBuf,
    lock_path: PathBuf,
    state: Cursor,
    /// In-memory only. Lost on restart *by design*: the persisted checkpoint
    /// was already held down to the oldest of these, so they are re-delivered.
    in_flight: BTreeMap<String, u64>,
    capacity: usize,
}

impl CursorStore {
    /// Open the cursor, or start a fresh one at `now` if none exists.
    ///
    /// Unlike the enrolment record, a missing cursor is **not** fail-closed. It
    /// carries no security property — losing it costs unreplayed history, not
    /// a weakened policy — and refusing to start would turn a deleted cache
    /// into an outage. Starting at `now` is the honest default: nothing before
    /// this moment is claimed to have been seen.
    ///
    /// A *corrupt* cursor is still an error: that is a damaged file, not a
    /// first run, and guessing a resume point from it would silently skip
    /// history.
    ///
    /// # Errors
    /// [`CursorError::Corrupt`] if the file exists but cannot be read.
    pub fn open_or_start(
        path: impl Into<PathBuf>,
        now: u64,
        capacity: usize,
    ) -> Result<Self, CursorError> {
        let path = path.into();
        let state = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| CursorError::Corrupt {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Cursor {
                checkpoint_secs: now,
                high_water_secs: now,
                completed: VecDeque::new(),
            },
            Err(e) => {
                return Err(CursorError::Corrupt {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })
            }
        };
        Ok(Self {
            lock_path: lock_path_for(&path),
            path,
            state,
            in_flight: BTreeMap::new(),
            capacity: capacity.max(1),
        })
    }

    /// The persisted state, as last read or written.
    #[must_use]
    pub fn state(&self) -> &Cursor {
        &self.state
    }

    /// Where to resume the subscription, and whether the gap is still useful.
    ///
    /// `since` is the checkpoint minus [`crate::RECONNECT_OVERLAP_SECS`],
    /// which is twice the relay's accepted drift — enough that a backdated
    /// event accepted after a future-dated one cannot fall behind the window.
    #[must_use]
    pub fn resume(&self, now: u64) -> Resume {
        let since = self
            .state
            .checkpoint_secs
            .saturating_sub(crate::RECONNECT_OVERLAP_SECS);
        let behind = now.saturating_sub(self.state.checkpoint_secs);
        let max_age = buzz_core::relay::REPLAY_FLOOR_MAX_AGE_SECS;
        if behind > max_age {
            return Resume::GapTooOld {
                since,
                behind_secs: behind,
                max_age_secs: max_age,
            };
        }
        Resume::Since(since)
    }

    /// Claim an event for processing, holding the checkpoint down to it.
    ///
    /// Persists before returning `Fresh`, so a crash immediately afterwards
    /// still resumes from at or before this event.
    ///
    /// # Errors
    /// [`CursorError::Persist`] if the lowered checkpoint could not be made
    /// durable — in which case the event is *not* claimed and the caller must
    /// not process it.
    pub fn admit(&mut self, event_id: &str, created_at: u64) -> Result<Admission, CursorError> {
        if self.in_flight.contains_key(event_id)
            || self.state.completed.iter().any(|id| id == event_id)
        {
            return Ok(Admission::Duplicate);
        }
        self.in_flight.insert(event_id.to_string(), created_at);
        let next = self.with_checkpoint(self.compute_checkpoint());
        if next.checkpoint_secs != self.state.checkpoint_secs {
            if let Err(e) = self.persist(&next) {
                // Do not hand out a claim we could not make durable.
                self.in_flight.remove(event_id);
                return Err(e);
            }
            self.state = next;
        }
        Ok(Admission::Fresh)
    }

    /// Record a terminal outcome and let the checkpoint advance.
    ///
    /// # Errors
    /// [`CursorError::NotInFlight`] if the event was never admitted;
    /// [`CursorError::Persist`] if the advance could not be made durable — the
    /// event then stays in flight and is retried after a restart.
    pub fn complete(&mut self, event_id: &str) -> Result<(), CursorError> {
        let Some(created_at) = self.in_flight.remove(event_id) else {
            return Err(CursorError::NotInFlight {
                event_id: event_id.to_string(),
            });
        };

        let mut next = self.state.clone();
        next.high_water_secs = next.high_water_secs.max(created_at);
        next.completed.push_back(event_id.to_string());
        while next.completed.len() > self.capacity {
            next.completed.pop_front();
        }
        // Recompute *after* removing this event from flight, so the checkpoint
        // is free to move up to the next-oldest unfinished event.
        next.checkpoint_secs = self.compute_checkpoint_with(next.high_water_secs);

        if let Err(e) = self.persist(&next) {
            self.in_flight.insert(event_id.to_string(), created_at);
            return Err(e);
        }
        self.state = next;
        Ok(())
    }

    /// Release a claim without completing it, so it is retried.
    ///
    /// Leaves the checkpoint held down until something else moves it, which is
    /// the conservative direction: an abandoned event must stay replayable.
    pub fn abandon(&mut self, event_id: &str) {
        self.in_flight.remove(event_id);
    }

    /// Events currently claimed.
    #[must_use]
    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    fn compute_checkpoint(&self) -> u64 {
        self.compute_checkpoint_with(self.state.high_water_secs)
    }

    /// The low-water mark: the oldest unfinished event, else the high water.
    fn compute_checkpoint_with(&self, high_water: u64) -> u64 {
        match self.in_flight.values().min() {
            Some(oldest) => (*oldest).min(high_water),
            None => high_water,
        }
    }

    fn with_checkpoint(&self, checkpoint_secs: u64) -> Cursor {
        Cursor {
            checkpoint_secs,
            ..self.state.clone()
        }
    }

    fn persist(&self, next: &Cursor) -> Result<(), CursorError> {
        let fail = |reason: String| CursorError::Persist {
            path: self.path.display().to_string(),
            reason,
        };
        let fence = FenceGuard::acquire(&self.lock_path).map_err(|e| fail(e.to_string()))?;
        let encoded = serde_json::to_vec(next).map_err(|e| fail(e.to_string()))?;
        atomic_write(&fence, &self.path, &encoded).map_err(|e| fail(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;

    fn store(dir: &tempfile::TempDir) -> CursorStore {
        CursorStore::open_or_start(dir.path().join("cursor.json"), NOW, DEFAULT_COMPLETED_RING)
            .expect("open")
    }

    fn reopen(dir: &tempfile::TempDir, now: u64) -> CursorStore {
        CursorStore::open_or_start(dir.path().join("cursor.json"), now, DEFAULT_COMPLETED_RING)
            .expect("reopen")
    }

    #[test]
    fn a_missing_cursor_starts_at_now() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(&dir);
        assert_eq!(s.state().checkpoint_secs, NOW);
    }

    #[test]
    fn a_corrupt_cursor_refuses_to_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cursor.json");
        fs::write(&path, b"{not json").expect("write");
        let err = CursorStore::open_or_start(&path, NOW, DEFAULT_COMPLETED_RING)
            .expect_err("must refuse");
        assert!(matches!(err, CursorError::Corrupt { .. }));
    }

    #[test]
    fn resume_subtracts_the_full_overlap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(&dir);
        assert_eq!(
            s.resume(NOW),
            Resume::Since(NOW - crate::RECONNECT_OVERLAP_SECS)
        );
    }

    #[test]
    fn a_second_admit_of_the_same_event_is_a_duplicate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        assert_eq!(s.admit("a", NOW).expect("admit"), Admission::Fresh);
        assert_eq!(s.admit("a", NOW).expect("re-admit"), Admission::Duplicate);
    }

    #[test]
    fn a_completed_event_is_not_re_admitted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        s.admit("a", NOW).expect("admit");
        s.complete("a").expect("complete");
        assert_eq!(s.admit("a", NOW).expect("re-admit"), Admission::Duplicate);
    }

    #[test]
    fn completing_an_unadmitted_event_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        assert!(matches!(
            s.complete("ghost").expect_err("must refuse"),
            CursorError::NotInFlight { .. }
        ));
    }

    /// **The reason the checkpoint is a low-water mark.**
    ///
    /// `A` is backdated a full drift and stays in flight; `B` is future-dated a
    /// full drift and completes. A max-completed cursor would jump to `B` and
    /// leave `A` behind the resume window forever. The checkpoint must stay at
    /// `A`.
    #[test]
    fn an_in_flight_event_holds_the_checkpoint_back_against_a_newer_completion() {
        let dir = tempfile::tempdir().expect("tempdir");
        let drift = buzz_core::relay::MAX_TIMESTAMP_DRIFT_SECS;
        let a_created = NOW - drift;
        let b_created = NOW + 700 + drift;

        let mut s = store(&dir);
        s.admit("a", a_created).expect("admit A");
        s.admit("b", b_created).expect("admit B");
        s.complete("b").expect("complete B");

        assert_eq!(
            s.state().checkpoint_secs,
            a_created,
            "the unfinished A must pin the checkpoint"
        );

        // Crash and restart: A must still be inside the resume window.
        let resumed = reopen(&dir, NOW + 700);
        let Resume::Since(since) = resumed.resume(NOW + 700) else {
            panic!("gap should not be stale here");
        };
        assert!(
            a_created >= since,
            "A at {a_created} must be re-delivered by since={since}"
        );
    }

    /// The same sequence proves the naive rule would have failed, so the test
    /// above is not passing by accident: max-completed lands past `A`.
    #[test]
    fn the_naive_max_completed_rule_would_have_skipped_that_event() {
        let drift = buzz_core::relay::MAX_TIMESTAMP_DRIFT_SECS;
        let a_created = NOW - drift;
        let naive_checkpoint = NOW + 700 + drift; // max completed
        let naive_since = naive_checkpoint - crate::RECONNECT_OVERLAP_SECS;
        assert!(
            a_created < naive_since,
            "precondition: max-completed ({naive_since}) skips A ({a_created})"
        );
    }

    #[test]
    fn the_checkpoint_advances_once_nothing_is_in_flight() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        s.admit("a", NOW + 10).expect("admit A");
        s.admit("b", NOW + 20).expect("admit B");
        s.complete("b").expect("complete B");
        assert_eq!(s.state().checkpoint_secs, NOW + 10);
        s.complete("a").expect("complete A");
        assert_eq!(s.state().checkpoint_secs, NOW + 20, "high water");
    }

    #[test]
    fn the_checkpoint_survives_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let mut s = store(&dir);
            s.admit("a", NOW + 50).expect("admit");
            s.complete("a").expect("complete");
        }
        assert_eq!(reopen(&dir, NOW + 60).state().high_water_secs, NOW + 50);
    }

    /// In-flight claims are deliberately not persisted: the checkpoint already
    /// holds down to the oldest, so a restart re-delivers them.
    #[test]
    fn an_in_flight_event_is_replayable_after_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let mut s = store(&dir);
            s.admit("a", NOW + 5).expect("admit");
            // crash: never completed
        }
        let resumed = reopen(&dir, NOW + 5);
        assert_eq!(resumed.in_flight_len(), 0);
        assert_eq!(
            resumed.admit_probe("a"),
            Admission::Fresh,
            "an uncompleted event must not be deduped away"
        );
    }

    #[test]
    fn abandoning_releases_the_claim_for_retry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        s.admit("a", NOW).expect("admit");
        s.abandon("a");
        assert_eq!(s.admit("a", NOW).expect("re-admit"), Admission::Fresh);
    }

    #[test]
    fn the_completed_ring_is_bounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s =
            CursorStore::open_or_start(dir.path().join("cursor.json"), NOW, 3).expect("open");
        for i in 0..6u64 {
            let id = format!("e{i}");
            s.admit(&id, NOW + i).expect("admit");
            s.complete(&id).expect("complete");
        }
        assert_eq!(s.state().completed.len(), 3);
        assert!(s.state().completed.iter().any(|id| id == "e5"));
        assert!(!s.state().completed.iter().any(|id| id == "e0"));
    }

    /// Past the replay-floor bound a woken agent could not be shown the
    /// mention, so this must surface rather than read as a healthy resume.
    #[test]
    fn a_gap_past_the_replay_floor_bound_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(&dir);
        let max_age = buzz_core::relay::REPLAY_FLOOR_MAX_AGE_SECS;
        let late = NOW + max_age + 1;
        match s.resume(late) {
            Resume::GapTooOld {
                behind_secs,
                max_age_secs,
                ..
            } => {
                assert_eq!(behind_secs, max_age + 1);
                assert_eq!(max_age_secs, max_age);
            }
            other => panic!("expected GapTooOld, got {other:?}"),
        }
    }

    #[test]
    fn a_gap_exactly_at_the_bound_is_still_usable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(&dir);
        let at_bound = NOW + buzz_core::relay::REPLAY_FLOOR_MAX_AGE_SECS;
        assert!(matches!(s.resume(at_bound), Resume::Since(_)));
    }

    impl CursorStore {
        /// Test-only: would this id be admitted, without claiming it?
        fn admit_probe(&self, event_id: &str) -> Admission {
            if self.in_flight.contains_key(event_id)
                || self.state.completed.iter().any(|id| id == event_id)
            {
                Admission::Duplicate
            } else {
                Admission::Fresh
            }
        }
    }
}
