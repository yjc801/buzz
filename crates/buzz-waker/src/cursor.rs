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
//! *Unfinished* means claimed and not yet terminal — which includes work
//! [`CursorStore::abandon`]ed after a retryable failure. Dropping an abandoned
//! event out of that set would let a newer completion raise the checkpoint past
//! it and reintroduce the exact loss above through the retry path.
//!
//! There is a second way an event can be unfinished without being claimed: it
//! is still queued in the relay's historical replay. Stored rows come back
//! newest-first, so during a drain the *oldest* rows are the last to arrive,
//! and a completed newer one must not raise the checkpoint past them. That is
//! what the replay pin between [`CursorStore::resume`] and
//! [`CursorStore::end_replay`] holds down.
//!
//! # Ordering
//!
//! The lowered checkpoint is persisted **before** the work starts
//! ([`CursorStore::admit`]) and raised only after a terminal outcome
//! ([`CursorStore::complete`]). Committing completion first would let a crash
//! mid-attempt replay the event and then dedupe it away unprocessed.
//! Processing-first is safe because provider deploy is idempotent — a
//! duplicate is a strict no-op.
//!
//! # Why this store owns its file for its whole lifetime
//!
//! The floor store shares a file safely by re-reading and deciding *inside*
//! the fence ([`crate::floors::FloorStore::with_fence`]). That does not
//! transfer here, because a checkpoint is a minimum over the in-flight set —
//! in-memory state that a second handle cannot observe. The merge that looks
//! conservative, `min(on_disk, mine)`, is a deadlock rather than a fix: a
//! handle that lowered the file for `A` and then completes `A` computes
//! `min(A_low, high_water)` and pins itself at its own stale value forever.
//!
//! So the cursor takes the fence once, in [`CursorStore::open_or_start`], and
//! holds it. A second handle is refused with [`CursorError::Locked`] instead of
//! silently overwriting a lowered checkpoint with its own cached high water and
//! making the first handle's in-flight event unrecoverable.
//!
//! # Measuring staleness
//!
//! `checkpoint_secs` is an event timestamp and can be skewed either way by the
//! relay's accepted drift, so it cannot measure how long the waker was gone.
//! [`Cursor::covered_through_secs`] is the separate real-time watermark that
//! can — the design's "runtime age alert" is about *real waker downtime*
//! (`PLANS/BUZZ_WAKER_DESIGN.md` §5).
//!
//! That watermark is necessary but not sufficient, because event age and
//! downtime are different quantities: a 1,100s outage can surface an event
//! backdated 899s that is 1,999s old. So each recovered event is *also* checked
//! against [`crate::WAKE_DELIVERABLE_AGE_SECS`] as it is admitted.

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

    /// Another live handle already owns the cursor file.
    ///
    /// Fatal by design: the second handle could not make a correct checkpoint
    /// decision anyway, since the first handle's in-flight set is in-memory.
    /// See the module note on lifetime ownership.
    #[error("cursor state at {path} is already owned by another handle; refusing to run a second")]
    Locked {
        /// The contended path.
        path: String,
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
        /// Real seconds since the feed was last known covered.
        behind_secs: u64,
        /// The bound it exceeded.
        max_age_secs: u64,
    },
}

/// Whether an event still needs processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Not seen before; now recorded in flight. Process it.
    Fresh,
    /// Claimed exactly as [`Admission::Fresh`], but old enough that the
    /// harness a wake starts is not guaranteed to be shown it.
    ///
    /// Still process it — the wake may well land, and skipping it would
    /// guarantee the loss instead of risking it. But the attempt must be
    /// reported as a failure rather than a healthy wake, for the reason given
    /// on [`Resume::GapTooOld`].
    FreshButUndeliverable {
        /// The event's real age at admission.
        age_secs: u64,
        /// The bound it exceeded, [`crate::WAKE_DELIVERABLE_AGE_SECS`].
        max_age_secs: u64,
    },
    /// Already completed, or already claimed. Skip it.
    Duplicate,
}

impl Admission {
    /// Whether this admission claimed the event, under either fresh variant.
    #[must_use]
    pub fn is_fresh(self) -> bool {
        !matches!(self, Admission::Duplicate)
    }
}

/// The persisted resume state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// Resume point: the low-water mark described in the module docs.
    pub checkpoint_secs: u64,
    /// Newest `created_at` that has reached a terminal outcome.
    pub high_water_secs: u64,
    /// Real time through which the feed is known to have been covered.
    ///
    /// Distinct from the two above, which are event timestamps carrying up to
    /// the relay's accepted drift in either direction. This one is the local
    /// clock at the last point the waker knew it was current, so
    /// `now - covered_through_secs` is real downtime and can be compared
    /// against an age bound.
    ///
    /// Only evidence that the feed is *current* may advance it: a live event
    /// ([`CursorStore::admit`] outside a replay drain), EOSE
    /// ([`CursorStore::end_replay`]), or an explicit heartbeat
    /// ([`CursorStore::mark_covered`]). Notably not [`CursorStore::complete`],
    /// and not a row arriving mid-drain — see those two for why.
    pub covered_through_secs: u64,
    /// Bounded ring of completed event ids, oldest first.
    pub completed: VecDeque<String>,
}

/// File-backed [`Cursor`], sole owner of its file for its lifetime.
#[derive(Debug)]
pub struct CursorStore {
    path: PathBuf,
    /// Held from `open_or_start` to drop — see the module note on ownership.
    fence: FenceGuard,
    state: Cursor,
    /// In-memory only. Lost on restart *by design*: the persisted checkpoint
    /// was already held down to the oldest of these, so they are re-delivered.
    in_flight: BTreeMap<String, u64>,
    /// Claimed, released for retry, still not terminal. Also in-memory only,
    /// and pins the checkpoint exactly as `in_flight` does.
    abandoned: BTreeMap<String, u64>,
    /// Ceiling on the checkpoint while historical replay is draining — see
    /// [`CursorStore::resume`]. In-memory for the same reason as the two sets
    /// above: what it protects is already on disk.
    replay_pin: Option<u64>,
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
    /// A fresh cursor is **persisted before returning**. Leaving it in memory
    /// would make the first claim undurable in the common case — a first live
    /// event is normally at or after `now`, so it does not lower the
    /// checkpoint, and a crash before it completes would leave no file at all.
    /// The next start would then begin at its own later `now` and step over the
    /// event. Writing at open also gives [`CursorStore::resume`] a coverage
    /// watermark from the very first run.
    ///
    /// # Errors
    /// [`CursorError::Corrupt`] if the file exists but cannot be read;
    /// [`CursorError::Locked`] if another handle owns it;
    /// [`CursorError::Persist`] if a fresh cursor could not be made durable.
    pub fn open_or_start(
        path: impl Into<PathBuf>,
        now: u64,
        capacity: usize,
    ) -> Result<Self, CursorError> {
        let path = path.into();
        let lock_path = lock_path_for(&path);
        let fence = FenceGuard::try_acquire(&lock_path)
            .map_err(|e| CursorError::Persist {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?
            .ok_or_else(|| CursorError::Locked {
                path: path.display().to_string(),
            })?;

        let (state, fresh) = match fs::read(&path) {
            Ok(bytes) => (
                serde_json::from_slice(&bytes).map_err(|e| CursorError::Corrupt {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })?,
                false,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
                Cursor {
                    checkpoint_secs: now,
                    high_water_secs: now,
                    covered_through_secs: now,
                    completed: VecDeque::new(),
                },
                true,
            ),
            Err(e) => {
                return Err(CursorError::Corrupt {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })
            }
        };

        let store = Self {
            path,
            fence,
            state,
            in_flight: BTreeMap::new(),
            abandoned: BTreeMap::new(),
            replay_pin: None,
            capacity: capacity.max(1),
        };
        if fresh {
            store.persist(&store.state)?;
        }
        Ok(store)
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
    ///
    /// Staleness is measured from [`Cursor::covered_through_secs`], never from
    /// the checkpoint: the checkpoint is an event timestamp that the relay's
    /// drift allows to sit up to 900s either side of real time, so a gap
    /// measured against it can read as healthy while the waker was in fact down
    /// for far longer. This bound is [`buzz_core::relay::REPLAY_FLOOR_MAX_AGE_SECS`]
    /// — an outage longer than that has certainly buried some of the window
    /// beyond any woken harness's reach, whatever the wake pipeline then costs.
    /// The narrower per-event question is [`CursorStore::admit`]'s.
    ///
    /// Asking this question **pins the checkpoint** at its current value until
    /// [`CursorStore::end_replay`], and that is why it takes `&mut self`. The
    /// low-water mark covers *admitted* work, and the history this `since` is
    /// about to request has not been admitted yet — the relay returns stored
    /// rows newest-first, so the oldest arrive last. Without the pin, a newer
    /// replayed row could complete and raise the checkpoint past an older row
    /// still queued behind it; a crash in that window would drop the older
    /// mention out of the next `since` for good. Pinning at the pre-REQ
    /// checkpoint — rather than at `since` — is exactly enough, because a
    /// restart re-derives the same `since` from it and so re-requests the same
    /// window, with no ratchet backwards on repeated crashes.
    pub fn resume(&mut self, now: u64) -> Resume {
        let since = self
            .state
            .checkpoint_secs
            .saturating_sub(crate::RECONNECT_OVERLAP_SECS);
        self.replay_pin = Some(self.state.checkpoint_secs);
        let behind = now.saturating_sub(self.state.covered_through_secs);
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

    /// Release the replay pin at EOSE, once all history has been admitted.
    ///
    /// Until this is called the checkpoint cannot rise above where
    /// [`CursorStore::resume`] left it, so forgetting the call costs re-delivery
    /// and eventually a stale-gap alert — never a lost mention. Calling it
    /// before the last historical row has been admitted is the unsafe
    /// direction, and is the caller's contract: EOSE is the relay's own
    /// statement that the batch is drained.
    ///
    /// # Errors
    /// [`CursorError::Persist`] if the released checkpoint could not be made
    /// durable — the pin then stays in place and replay is repeated.
    pub fn end_replay(&mut self, now: u64) -> Result<(), CursorError> {
        if self.replay_pin.is_none() {
            return Ok(());
        }
        let previous = self.replay_pin.take();
        let mut next = self.state.clone();
        next.checkpoint_secs = self.compute_checkpoint();
        next.covered_through_secs = next.covered_through_secs.max(now);
        if let Err(e) = self.persist(&next) {
            self.replay_pin = previous;
            return Err(e);
        }
        self.state = next;
        Ok(())
    }

    /// Record that the feed is covered through `now`, without any event.
    ///
    /// The connection loop must call this while it is connected and idle:
    /// [`CursorStore::resume`] cannot tell a quiet period from an outage, so a
    /// silent stretch longer than [`buzz_core::relay::REPLAY_FLOOR_MAX_AGE_SECS`]
    /// raises a false stale-gap alert on the next restart. Erring that way is
    /// deliberate — the alternative is an outage that reads as healthy.
    ///
    /// # Errors
    /// [`CursorError::Persist`] if the watermark could not be made durable.
    pub fn mark_covered(&mut self, now: u64) -> Result<(), CursorError> {
        if now <= self.state.covered_through_secs {
            return Ok(());
        }
        let mut next = self.state.clone();
        next.covered_through_secs = now;
        self.persist(&next)?;
        self.state = next;
        Ok(())
    }

    /// Claim an event for processing, holding the checkpoint down to it.
    ///
    /// Persists before returning, so a crash immediately afterwards still
    /// resumes from at or before this event.
    ///
    /// A claimed event is reported as [`Admission::FreshButUndeliverable`]
    /// when it is already older than [`crate::WAKE_DELIVERABLE_AGE_SECS`] —
    /// recovering it is still worth doing, but the wake it triggers cannot be
    /// called healthy.
    ///
    /// # Errors
    /// [`CursorError::Persist`] if the lowered checkpoint could not be made
    /// durable — in which case the event is *not* claimed and the caller must
    /// not process it.
    pub fn admit(
        &mut self,
        event_id: &str,
        created_at: u64,
        now: u64,
    ) -> Result<Admission, CursorError> {
        if self.in_flight.contains_key(event_id)
            || self.state.completed.iter().any(|id| id == event_id)
        {
            return Ok(Admission::Duplicate);
        }
        // Reclaiming an abandoned event is a fresh claim, not a duplicate: it
        // was released precisely so it would be retried.
        let previous = self.abandoned.remove(event_id);
        let created_at = previous.map_or(created_at, |was| was.min(created_at));
        self.in_flight.insert(event_id.to_string(), created_at);

        let mut next = self.state.clone();
        next.checkpoint_secs = self.compute_checkpoint();
        if self.replay_pin.is_none() {
            // A *live* event proves the feed is current through `now`. A row
            // arriving during a replay drain proves only that the connection
            // is up, not that everything before it has been seen — the batch
            // is still landing, and EOSE is the relay's own statement that it
            // is not. `end_replay` is where a drain's coverage is recorded.
            next.covered_through_secs = next.covered_through_secs.max(now);
        }
        if let Err(e) = self.persist(&next) {
            // Do not hand out a claim we could not make durable.
            self.in_flight.remove(event_id);
            if let Some(was) = previous {
                self.abandoned.insert(event_id.to_string(), was);
            }
            return Err(e);
        }
        self.state = next;

        let age = now.saturating_sub(created_at);
        if age > crate::WAKE_DELIVERABLE_AGE_SECS {
            return Ok(Admission::FreshButUndeliverable {
                age_secs: age,
                max_age_secs: crate::WAKE_DELIVERABLE_AGE_SECS,
            });
        }
        Ok(Admission::Fresh)
    }

    /// Record a terminal outcome and let the checkpoint advance.
    ///
    /// Terminal covers both a finished wake and a caller giving up on one:
    /// an [`CursorStore::abandon`]ed event may be completed directly, which is
    /// the only way to release its hold on the checkpoint without processing
    /// it. Retryable failures use `abandon` and leave the hold in place.
    ///
    /// Deliberately takes no `now`: a wake outlives its connection, so the
    /// moment one finishes says nothing about whether the feed was live for
    /// it. Feeding that time into [`Cursor::covered_through_secs`] would let a
    /// wake that ran straight through an outage report the outage as shorter
    /// than it was and suppress the alert. Not accepting the argument is what
    /// keeps that from being reintroduced.
    ///
    /// # Errors
    /// [`CursorError::NotInFlight`] if the event was never admitted;
    /// [`CursorError::Persist`] if the advance could not be made durable — the
    /// event then stays claimed and is retried after a restart.
    pub fn complete(&mut self, event_id: &str) -> Result<(), CursorError> {
        let was_abandoned = self.abandoned.contains_key(event_id);
        let Some(created_at) = self
            .in_flight
            .remove(event_id)
            .or_else(|| self.abandoned.remove(event_id))
        else {
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
        // Recompute *after* dropping this event, so the checkpoint is free to
        // move up to the next-oldest unfinished one.
        next.checkpoint_secs = self.compute_checkpoint_with(next.high_water_secs);

        if let Err(e) = self.persist(&next) {
            let restore = if was_abandoned {
                &mut self.abandoned
            } else {
                &mut self.in_flight
            };
            restore.insert(event_id.to_string(), created_at);
            return Err(e);
        }
        self.state = next;
        Ok(())
    }

    /// Release a claim without completing it, so it is retried.
    ///
    /// The event keeps holding the checkpoint down until it is reclaimed or
    /// completed. Dropping it out of the unfinished set instead would let the
    /// next completion raise the checkpoint past it, which is the module's
    /// headline loss reached through the retry path: a crash after that raise
    /// leaves the abandoned event behind the resume window, unreplayable,
    /// despite this method promising the opposite.
    pub fn abandon(&mut self, event_id: &str) {
        if let Some(created_at) = self.in_flight.remove(event_id) {
            self.abandoned.insert(event_id.to_string(), created_at);
        }
    }

    /// Events currently being processed.
    #[must_use]
    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    /// Events released for retry and still holding the checkpoint down.
    #[must_use]
    pub fn abandoned_len(&self) -> usize {
        self.abandoned.len()
    }

    fn compute_checkpoint(&self) -> u64 {
        self.compute_checkpoint_with(self.state.high_water_secs)
    }

    /// The low-water mark: the oldest unfinished event, else the high water —
    /// and never above an active replay pin, which stands in for the history
    /// that has been requested but not yet admitted.
    fn compute_checkpoint_with(&self, high_water: u64) -> u64 {
        let unfinished = self
            .in_flight
            .values()
            .chain(self.abandoned.values())
            .chain(self.replay_pin.iter())
            .min();
        match unfinished {
            Some(oldest) => (*oldest).min(high_water),
            None => high_water,
        }
    }

    fn persist(&self, next: &Cursor) -> Result<(), CursorError> {
        let fail = |reason: String| CursorError::Persist {
            path: self.path.display().to_string(),
            reason,
        };
        let encoded = serde_json::to_vec(next).map_err(|e| fail(e.to_string()))?;
        atomic_write(&self.fence, &self.path, &encoded).map_err(|e| fail(e.to_string()))?;
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

    /// A second handle cannot make a correct checkpoint decision — the first
    /// handle's in-flight set is in memory — so it must be refused rather than
    /// left to overwrite a lowered checkpoint with its own high water.
    #[test]
    fn a_second_handle_is_refused_while_the_first_is_alive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = store(&dir);
        let err =
            CursorStore::open_or_start(dir.path().join("cursor.json"), NOW, DEFAULT_COMPLETED_RING)
                .expect_err("second handle must be refused");
        assert!(matches!(err, CursorError::Locked { .. }));
        drop(first);
        reopen(&dir, NOW); // released on drop
    }

    /// The concrete lost update the refusal prevents: without it, B's cached
    /// high water would replace A's lowered checkpoint on disk and A's
    /// in-flight event would never be re-delivered.
    #[test]
    fn a_stale_handle_cannot_overwrite_a_lowered_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = NOW - buzz_core::relay::MAX_TIMESTAMP_DRIFT_SECS;

        let mut a = store(&dir);
        a.admit("a", old, NOW).expect("A admits an old event");
        assert!(CursorStore::open_or_start(
            dir.path().join("cursor.json"),
            NOW + 5_000,
            DEFAULT_COMPLETED_RING
        )
        .is_err());

        drop(a);
        assert_eq!(
            reopen(&dir, NOW).state().checkpoint_secs,
            old,
            "the lowered checkpoint must survive"
        );
    }

    /// Finding 4: the first live event is normally at or after `now`, so it
    /// does not lower the checkpoint and nothing else forces a write. If open
    /// did not persist, a crash here would leave no file and the next start
    /// would begin at its own later `now`, stepping over the event.
    #[test]
    fn a_fresh_cursor_is_durable_before_any_event_completes() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let mut s = store(&dir);
            s.admit("a", NOW + 5, NOW).expect("admit");
            // crash: never completed
        }
        let much_later = NOW + 50_000;
        let mut resumed = reopen(&dir, much_later);
        assert_eq!(resumed.state().checkpoint_secs, NOW);
        let (Resume::Since(since) | Resume::GapTooOld { since, .. }) = resumed.resume(much_later);
        assert!(
            NOW + 5 >= since,
            "the uncompleted event at {} must still be inside since={since}",
            NOW + 5
        );
    }

    /// The other half of open-time durability, which no admission can cover:
    /// with no file on disk, a restart resets the coverage watermark to its own
    /// `now`, so an outage across the very first window reads as healthy no
    /// matter how long it lasted.
    #[test]
    fn an_outage_before_the_first_event_is_still_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        drop(store(&dir)); // starts, covers nothing, dies
        let late = NOW + buzz_core::relay::REPLAY_FLOOR_MAX_AGE_SECS + 1;
        let mut resumed = reopen(&dir, late);
        assert!(
            matches!(resumed.resume(late), Resume::GapTooOld { .. }),
            "the first run's coverage must be durable too"
        );
    }

    #[test]
    fn resume_subtracts_the_full_overlap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        assert_eq!(
            s.resume(NOW),
            Resume::Since(NOW - crate::RECONNECT_OVERLAP_SECS)
        );
    }

    #[test]
    fn a_second_admit_of_the_same_event_is_a_duplicate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        assert_eq!(s.admit("a", NOW, NOW).expect("admit"), Admission::Fresh);
        assert_eq!(
            s.admit("a", NOW, NOW).expect("re-admit"),
            Admission::Duplicate
        );
    }

    #[test]
    fn a_completed_event_is_not_re_admitted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        s.admit("a", NOW, NOW).expect("admit");
        s.complete("a").expect("complete");
        assert_eq!(
            s.admit("a", NOW, NOW).expect("re-admit"),
            Admission::Duplicate
        );
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
        s.admit("a", a_created, NOW).expect("admit A");
        s.admit("b", b_created, NOW).expect("admit B");
        s.complete("b").expect("complete B");

        assert_eq!(
            s.state().checkpoint_secs,
            a_created,
            "the unfinished A must pin the checkpoint"
        );

        // Crash and restart: A must still be inside the resume window.
        drop(s);
        let mut resumed = reopen(&dir, NOW + 700);
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
        s.admit("a", NOW + 10, NOW).expect("admit A");
        s.admit("b", NOW + 20, NOW).expect("admit B");
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
            s.admit("a", NOW + 50, NOW).expect("admit");
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
            s.admit("a", NOW + 5, NOW).expect("admit");
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
        s.admit("a", NOW, NOW).expect("admit");
        s.abandon("a");
        assert_eq!(s.admit("a", NOW, NOW).expect("re-admit"), Admission::Fresh);
        assert_eq!(s.abandoned_len(), 0, "reclaimed, not still released");
    }

    /// The headline loss sequence reached through the retry path: `A` is
    /// released for retry rather than completed, so it is still unfinished and
    /// must keep pinning the checkpoint against `B`'s completion.
    #[test]
    fn an_abandoned_event_still_pins_the_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let drift = buzz_core::relay::MAX_TIMESTAMP_DRIFT_SECS;
        let a_created = NOW - drift;
        let b_created = NOW + 700 + drift;

        let mut s = store(&dir);
        s.admit("a", a_created, NOW).expect("admit A");
        s.abandon("a"); // retryable failure
        s.admit("b", b_created, NOW).expect("admit B");
        s.complete("b").expect("complete B");
        assert_eq!(
            s.state().checkpoint_secs,
            a_created,
            "the abandoned A must still pin the checkpoint"
        );

        // Crash before the retry runs: A must still be inside the window.
        drop(s);
        let mut resumed = reopen(&dir, NOW + 700);
        let (Resume::Since(since) | Resume::GapTooOld { since, .. }) = resumed.resume(NOW + 700);
        assert!(
            a_created >= since,
            "A at {a_created} must be re-delivered by since={since}"
        );
    }

    /// Giving up permanently is a terminal outcome, and the only way to
    /// release the hold without processing the event.
    #[test]
    fn completing_an_abandoned_event_releases_its_pin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        s.admit("a", NOW - 100, NOW).expect("admit A");
        s.admit("b", NOW + 20, NOW).expect("admit B");
        s.complete("b").expect("complete B");
        s.abandon("a");
        assert_eq!(s.abandoned_len(), 1);
        assert_eq!(s.state().checkpoint_secs, NOW - 100);

        s.complete("a").expect("give up on A");
        assert_eq!(s.abandoned_len(), 0);
        assert_eq!(s.state().checkpoint_secs, NOW + 20, "high water");
        assert_eq!(
            s.admit("a", NOW - 100, NOW).expect("re-admit"),
            Admission::Duplicate,
            "a terminal event is deduped"
        );
    }

    #[test]
    fn the_completed_ring_is_bounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s =
            CursorStore::open_or_start(dir.path().join("cursor.json"), NOW, 3).expect("open");
        for i in 0..6u64 {
            let id = format!("e{i}");
            s.admit(&id, NOW + i, NOW).expect("admit");
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
        let mut s = store(&dir);
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
        let mut s = store(&dir);
        let at_bound = NOW + buzz_core::relay::REPLAY_FLOOR_MAX_AGE_SECS;
        assert!(matches!(s.resume(at_bound), Resume::Since(_)));
    }

    /// A checkpoint is an event timestamp and can be future-dated by a full
    /// drift, so measuring downtime against it under-reports by that much. Two
    /// hours down with a checkpoint 900s ahead would read as 6,300s — inside
    /// the bound is not the point; the quantity itself is wrong.
    #[test]
    fn staleness_is_measured_from_real_coverage_not_the_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let drift = buzz_core::relay::MAX_TIMESTAMP_DRIFT_SECS;
        let bound = buzz_core::relay::REPLAY_FLOOR_MAX_AGE_SECS;
        {
            let mut s = store(&dir);
            // A future-dated event pushes the checkpoint a full drift ahead.
            s.admit("future", NOW + drift, NOW).expect("admit");
            s.complete("future").expect("complete");
            assert_eq!(s.state().checkpoint_secs, NOW + drift);
        }

        // Down for just over the bound, measured in real time.
        let back = NOW + bound + 1;
        let mut resumed = reopen(&dir, back);
        assert!(
            matches!(resumed.resume(back), Resume::GapTooOld { .. }),
            "an outage past the bound must alert; against the checkpoint it \
             would have measured {} and read as healthy",
            back - (NOW + drift)
        );
    }

    /// A wake outlives its connection: it can still be running minutes after
    /// the relay dropped. Its completion time is evidence that processing
    /// finished, never that the feed was live for it, so counting it as
    /// coverage would let a wake that ran straight through an outage report
    /// that outage as shorter than it was and suppress the alert.
    #[test]
    fn a_completed_wake_is_not_evidence_that_the_feed_was_covered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bound = buzz_core::relay::REPLAY_FLOOR_MAX_AGE_SECS;
        {
            let mut s = store(&dir);
            s.admit("a", NOW, NOW).expect("admit"); // delivered: covered to NOW
                                                    // The relay drops here. The wake keeps running for another 1,000s
                                                    // and only then reaches a terminal outcome.
            s.complete("a").expect("complete");
            assert_eq!(
                s.state().covered_through_secs,
                NOW,
                "completion must not move the coverage watermark"
            );
        }

        // The feed was uncovered for the whole span, and that must surface.
        let back = NOW + bound + 1;
        let mut resumed = reopen(&dir, back);
        assert!(
            matches!(resumed.resume(back), Resume::GapTooOld { .. }),
            "an outage a wake ran through is still an outage"
        );
    }

    /// The same principle one step earlier: a row delivered while the drain is
    /// still running says the connection is up, not that the backlog is gone.
    /// Counting it as coverage would let a crash mid-drain clear the alert for
    /// a window that has not been drained.
    #[test]
    fn a_row_arriving_mid_drain_is_not_evidence_the_feed_is_current() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bound = buzz_core::relay::REPLAY_FLOOR_MAX_AGE_SECS;
        let back = NOW + bound + 1;
        {
            let mut s = store(&dir);
            // Back after an outage past the bound: the drain starts here.
            assert!(matches!(s.resume(back), Resume::GapTooOld { .. }));

            s.admit("replayed", NOW + 10, back).expect("admit");
            assert_eq!(
                s.state().covered_through_secs,
                NOW,
                "a mid-drain row must not clear the outage"
            );
            assert!(
                matches!(s.resume(back), Resume::GapTooOld { .. }),
                "still uncovered until EOSE"
            );

            s.end_replay(back).expect("eose");
            assert_eq!(s.state().covered_through_secs, back, "EOSE covers it");
        }
        let mut after = reopen(&dir, back + 1);
        assert!(matches!(after.resume(back + 1), Resume::Since(_)));
    }

    #[test]
    fn marking_coverage_advances_the_watermark_monotonically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        s.mark_covered(NOW + 100).expect("advance");
        assert_eq!(s.state().covered_through_secs, NOW + 100);
        s.mark_covered(NOW + 50).expect("stale mark");
        assert_eq!(s.state().covered_through_secs, NOW + 100, "never goes back");
        drop(s);
        assert_eq!(reopen(&dir, NOW).state().covered_through_secs, NOW + 100);
    }

    /// Alex's sequence, and the reason the coverage watermark alone is not
    /// enough: an outage well inside the bound can still surface an event whose
    /// own age is past it, because the relay accepts backdating.
    #[test]
    fn a_recovered_event_past_the_deliverable_age_is_flagged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let drift = buzz_core::relay::MAX_TIMESTAMP_DRIFT_SECS;
        let mut s = store(&dir);

        let back = NOW + 1_100; // 1,100s of downtime — inside the resume bound
        assert!(matches!(s.resume(back), Resume::Since(_)));

        let recovered = NOW - drift + 1; // backdated a full drift at acceptance
        match s.admit("a", recovered, back).expect("admit") {
            Admission::FreshButUndeliverable {
                age_secs,
                max_age_secs,
            } => {
                assert_eq!(age_secs, back - recovered);
                assert_eq!(max_age_secs, crate::WAKE_DELIVERABLE_AGE_SECS);
            }
            other => panic!("expected the age to be flagged, got {other:?}"),
        }
        assert_eq!(s.in_flight_len(), 1, "flagged, but still claimed");
    }

    /// A live trigger is at most the relay's past skew old when it arrives, so
    /// the ordinary path must never be flagged — see the constant's docs.
    #[test]
    fn a_live_event_at_the_relay_skew_limit_is_not_flagged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        let oldest_live = NOW - buzz_core::relay::MAX_TIMESTAMP_DRIFT_SECS;
        assert_eq!(
            s.admit("a", oldest_live, NOW).expect("admit"),
            Admission::Fresh
        );
    }

    /// Historical rows arrive newest-first. Completing the newer one must not
    /// raise the checkpoint past an older row that has not been delivered yet.
    #[test]
    fn replay_holds_the_checkpoint_until_eose() {
        let dir = tempfile::tempdir().expect("tempdir");
        let drift = buzz_core::relay::MAX_TIMESTAMP_DRIFT_SECS;
        let checkpoint = NOW + drift;
        {
            let mut seed = store(&dir);
            seed.admit("seed", checkpoint, NOW).expect("admit");
            seed.complete("seed").expect("complete");
            assert_eq!(seed.state().checkpoint_secs, checkpoint);
        }

        let older = NOW - drift; // still queued behind the newer row
        let newer = NOW + 2_000;
        let mut s = reopen(&dir, NOW);
        let Resume::Since(since) = s.resume(NOW) else {
            panic!("gap should not be stale here");
        };
        assert!(older >= since, "precondition: the REQ covers the older row");

        // The relay delivers `newer` first and it completes quickly.
        s.admit("newer", newer, NOW).expect("admit");
        s.complete("newer").expect("complete");
        assert_eq!(
            s.state().checkpoint_secs,
            checkpoint,
            "the pin must hold the checkpoint at its pre-REQ value"
        );

        // Crash before `older` is ever delivered.
        drop(s);
        let mut resumed = reopen(&dir, NOW);
        let Resume::Since(next_since) = resumed.resume(NOW) else {
            panic!("gap should not be stale here");
        };
        assert_eq!(next_since, since, "the same window is re-requested");
        assert!(
            older >= next_since,
            "the undelivered row at {older} must survive since={next_since}"
        );
    }

    /// The pin releases at EOSE and the checkpoint takes the advance it earned.
    #[test]
    fn eose_releases_the_pin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        s.resume(NOW);
        s.admit("a", NOW + 40, NOW).expect("admit");
        s.complete("a").expect("complete");
        assert_eq!(s.state().checkpoint_secs, NOW, "pinned during replay");

        s.end_replay(NOW).expect("eose");
        assert_eq!(s.state().checkpoint_secs, NOW + 40);
        drop(s);
        assert_eq!(reopen(&dir, NOW).state().checkpoint_secs, NOW + 40);
    }

    /// An event still being worked outlives the replay batch, so releasing the
    /// pin must not let the checkpoint jump over it.
    #[test]
    fn eose_does_not_release_an_in_flight_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        s.resume(NOW);
        s.admit("slow", NOW + 10, NOW).expect("admit slow");
        s.admit("fast", NOW + 90, NOW).expect("admit fast");
        s.complete("fast").expect("complete fast");
        s.end_replay(NOW).expect("eose");
        assert_eq!(
            s.state().checkpoint_secs,
            NOW + 10,
            "the unfinished event still pins the checkpoint"
        );
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
