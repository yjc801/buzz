//! The wake **attempt** — one agent, one decision, one deploy.
//!
//! Ported from `runWakeAttempt` in
//! `desktop/src/features/agents/lib/agentWake.ts`, the effectful half of the
//! decision core whose pure half is [`crate::decide`]. Where `decide` answers
//! *should this event wake this agent*, this module answers *and did waking it
//! actually work* — which is where every failure mode that reached review
//! lives: double-firing on a burst, deploying on a relay hiccup, deploying an
//! agent that is already up, and trusting a status no live process is behind.
//!
//! As with `decide`, the desktop keeps its copy, so **this is a second
//! implementation of one decision and the two must not drift.** Every rule
//! below carries the reason it exists; none of them are obvious and nearly all
//! were found by a specific failure.
//!
//! # The shape, and why it is this shape
//!
//! Nothing here touches a clock, a relay or a provider directly: every effect
//! arrives through [`WakeEffects`]. That is not test decoration — the attempt
//! is a sequence of decisions *about* effects (has enough time passed, did a
//! beat land after the deploy, has this attempt's generation been cancelled),
//! and injecting them is what makes the sequence assertable without a live
//! agent and a two-minute wall clock.
//!
//! # The one rule worth reading before the code
//!
//! **The only accepted proof that a harness is alive is two distinct heartbeat
//! deliveries after this attempt began, spaced in *local* time**
//! ([`LiveEvidenceTracker`]). Status is never trusted: a crashed harness's last
//! `online` survives in the store for the presence TTL. A pre-attempt beat
//! proves nothing — the harness can crash a second after publishing it. A
//! single post-attempt beat proves nothing either: it can be a dying
//! generation's last in-flight beat, and its `created_at` rides a remote clock
//! the relay lets drift ±15 minutes, so no timestamp comparison can rescue it.
//! Only a process still running emits a *second*, spaced beat.
//!
//! Unproven is not fatal, because deploy is idempotent: the attempt deploys
//! anyway and the provider treats a live agent as a strict no-op. The cost of
//! being wrong in that direction is one round trip; the cost of being wrong in
//! the other is a mention nobody answers.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Mutex, PoisonError};

use buzz_core::PresenceStatus;

use crate::decide::normalize_pubkey;

/// How long a wake attempt suppresses the next one for the same agent.
///
/// Sized against a **cold start**, not against a double-send. A provider agent
/// is not live the instant `deploy` returns: the substrate still has to start
/// the harness, and the harness only then publishes presence. Every mention
/// arriving inside that gap would otherwise read as "still offline" and fire
/// its own redundant deploy.
pub const WAKE_ATTEMPT_DEBOUNCE_MS: u64 = 120_000;

/// Post-deploy convergence poll interval.
pub const WAKE_CONFIRM_POLL_MS: u64 = 5_000;

/// Post-deploy convergence attempts — `× WAKE_CONFIRM_POLL_MS` = 120s, the
/// same cold-start bound the debounce is sized to.
pub const WAKE_CONFIRM_ATTEMPTS: u32 = 24;

/// Liveness-evidence poll interval.
pub const WAKE_LIVE_EVIDENCE_POLL_MS: u64 = 5_000;

/// How many polls an `online` status gets to prove itself — `× POLL` = 135s,
/// which is ≥ two 60s heartbeat intervals, because the proof needs two beats.
pub const WAKE_LIVE_EVIDENCE_ATTEMPTS: u32 = 27;

/// Early bailout: if not even *one* post-attempt beat has landed by here
/// (75s > one heartbeat interval plus margin), the store entry is a crashed
/// harness's residue and waiting out the full window buys nothing.
pub const WAKE_LIVE_NO_BEAT_BAILOUT_ATTEMPTS: u32 = 15;

/// Minimum **local** delivery spacing between the two beats that constitute
/// proof. A dying generation has at most one final in-flight beat to land
/// late, whatever its clock claims; it cannot produce a second one this far
/// behind the first.
pub const WAKE_EVIDENCE_MIN_SPACING_MS: u64 = 30_000;

/// The relay's graceful-teardown window: an agent's process can outlive its
/// own `offline` publish by this long.
///
/// Mirrors the desktop's `REMOTE_POST_OFFLINE_GRACE_MS`. Deploying inside the
/// window strict-no-ops against the dying process, which looks exactly like a
/// successful wake and leaves the mention unanswered.
pub const REMOTE_POST_OFFLINE_GRACE_MS: u64 = 10_000;

/// Bound on triggers buffered while wake prerequisites are still resolving at
/// startup. The event tap has no history replay, so an unevaluable event must
/// be held rather than dropped — but only boundedly.
pub const WAKE_PENDING_TRIGGER_LIMIT: usize = 64;

/// Bound on triggers retained per agent behind an in-flight attempt, for retry
/// if the owning attempt exits without covering them.
pub const WAKE_COLLAPSED_TRIGGER_LIMIT: usize = 16;

/// When a settlement retains uncovered stragglers, one re-drive is scheduled
/// this far out — just past the deploy debounce, so the retry is never refused
/// as a hammer and a dead-again agent gets a real deploy that folds the
/// stragglers into its replay floor.
pub const WAKE_STRANDED_RETRY_DELAY_MS: u64 = WAKE_ATTEMPT_DEBOUNCE_MS + 5_000;

// The constants above are not independent — each of the following is a
// relationship the wake logic depends on, and a plausible-looking tweak to one
// number can silently break it. They are `const` assertions rather than tests
// on purpose: a violated relationship should refuse to compile, not wait for
// someone to run the suite.

/// If convergence gave up before the debounce expired, an unconfirmed wake
/// would release a stamp that had already lapsed — harmless — but a wake
/// confirmed after that point would be attributed to a retry instead of to the
/// attempt that actually caused it.
const _: () = assert!(
    WAKE_CONFIRM_ATTEMPTS as u64 * WAKE_CONFIRM_POLL_MS >= WAKE_ATTEMPT_DEBOUNCE_MS,
    "convergence must not give up before the debounce does"
);

/// Two spaced beats are the proof, so the evidence window has to be wide
/// enough for a live harness to actually produce them.
const _: () = assert!(
    WAKE_LIVE_EVIDENCE_ATTEMPTS as u64 * WAKE_LIVE_EVIDENCE_POLL_MS
        >= 2 * WAKE_EVIDENCE_MIN_SPACING_MS,
    "the evidence window must fit two spaced beats"
);

/// The bailout is an early exit from that window, and it has to sit beyond a
/// single heartbeat interval — cut it shorter and a healthy agent that simply
/// had not beaten yet gets redeployed.
const _: () = assert!(
    WAKE_LIVE_NO_BEAT_BAILOUT_ATTEMPTS < WAKE_LIVE_EVIDENCE_ATTEMPTS
        && WAKE_LIVE_NO_BEAT_BAILOUT_ATTEMPTS as u64 * WAKE_LIVE_EVIDENCE_POLL_MS
            > WAKE_EVIDENCE_MIN_SPACING_MS,
    "the bailout must be an early exit that still outlasts one heartbeat interval"
);

/// A stranded-trigger retry inside the debounce window would be refused as a
/// hammer, and the straggler it carries would never reach a fresh replay floor.
const _: () = assert!(
    WAKE_STRANDED_RETRY_DELAY_MS > WAKE_ATTEMPT_DEBOUNCE_MS,
    "the stranded retry must land past the debounce"
);

/// Why a wake attempt ended.
///
/// Every arm is a normal outcome except [`WakeOutcome::DeployFailed`] and
/// [`WakeOutcome::WakeUnconfirmed`]. "The agent was already up" is the *common*
/// case, not an error — any mention of a healthy agent reaches this path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WakeOutcome {
    /// Deploy accepted and a fresh generation's heartbeat followed it.
    Woken,
    /// Liveness was proven; no deploy was spent.
    AlreadyLive,
    /// A recent attempt for this agent already covers the mention.
    Debounced,
    /// Another attempt for this agent owns the decision.
    InFlight,
    /// The presence lookup failed. Deliberately **not** treated as "offline":
    /// an outage would otherwise deploy on every relay hiccup.
    PresenceUnavailable,
    /// The provider refused the deploy.
    DeployFailed,
    /// The deploy was accepted but no post-attempt heartbeat ever appeared.
    /// The attempt has already released its debounce so the next mention can
    /// retry.
    WakeUnconfirmed,
    /// The pre-deploy re-check identified the author as a known agent. No
    /// deploy is spent and the debounce is not stamped.
    AuthorRejected,
    /// The pre-deploy author re-check could not be completed. Fails closed,
    /// same as rejection, and likewise does not stamp.
    AuthorUnverified,
    /// The attempt's generation fence fired. Always quiet, and guaranteed
    /// *before* any external effect that would act on the successor
    /// generation's workspace.
    Cancelled,
}

impl WakeOutcome {
    /// The stable string form, matching the desktop's union members exactly so
    /// logs and any future cross-process comparison line up.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Woken => "woken",
            Self::AlreadyLive => "already-live",
            Self::Debounced => "debounced",
            Self::InFlight => "in-flight",
            Self::PresenceUnavailable => "presence-unavailable",
            Self::DeployFailed => "deploy-failed",
            Self::WakeUnconfirmed => "wake-unconfirmed",
            Self::AuthorRejected => "author-rejected",
            Self::AuthorUnverified => "author-unverified",
            Self::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for WakeOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How one attempt ended, plus the context a caller needs to react.
#[derive(Debug)]
pub struct WakeAttemptResult<E> {
    /// Why the attempt ended.
    pub outcome: WakeOutcome,
    /// The attempt deployed against a status that still claimed `online`, so
    /// the deploy was reconciliation against possibly-stale state.
    ///
    /// The caller uses this to suppress the "waking up" surface — but **not**
    /// to suppress failures: reconcile also covers the dead-agent case, and
    /// staying quiet there would silently lose the mention.
    pub reconcile: bool,
    /// The underlying effect error, for the two outcomes that carry one.
    pub error: Option<E>,
    /// The **provider's** proof that this attempt's deploy started a fresh
    /// harness generation, when a deploy was actually spent.
    ///
    /// `Some(true)` — the env this deploy carried (the replay floor
    /// included) is provably in effect. `Some(false)` — the deploy was a
    /// strict no-op against an already-running generation, so a subsequent
    /// heartbeat proving [`WakeOutcome::Woken`] can be that *old*
    /// generation's beat, not evidence the replay floor took effect. `None`
    /// — no deploy was spent, or the provider gave no classification, which
    /// is likewise unproven. Never used to change `outcome` itself: deploy
    /// is idempotent by contract, so a strict no-op is still a legitimate
    /// settlement — this field only tells a caller whether it may *also*
    /// presume the mention that caused it was delivered.
    pub floor_adopted: Option<bool>,
}

impl<E> WakeAttemptResult<E> {
    fn plain(outcome: WakeOutcome, reconcile: bool) -> Self {
        Self {
            outcome,
            reconcile,
            error: None,
            floor_adopted: None,
        }
    }

    fn failed(outcome: WakeOutcome, reconcile: bool, error: E) -> Self {
        Self {
            outcome,
            reconcile,
            error: Some(error),
            floor_adopted: None,
        }
    }

    fn woken(reconcile: bool, floor_adopted: Option<bool>) -> Self {
        Self {
            outcome: WakeOutcome::Woken,
            reconcile,
            error: None,
            floor_adopted,
        }
    }
}

/// One observed heartbeat delivery.
///
/// `observed_at_ms` is *this* machine's clock at delivery, never the event's
/// `created_at`: the relay accepts ±15 minutes of emitter drift, so an emitter
/// timestamp can be ordered against nothing local.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatObservation {
    /// The heartbeat event's id — the identity half of "two *distinct* beats".
    pub event_id: String,
    /// Local delivery time, milliseconds.
    pub observed_at_ms: u64,
}

/// Clock-free liveness proof for one wake attempt.
///
/// Proof requires two distinct heartbeat events, both delivered (local clock)
/// at or after `since_ms`, with the later delivery at least `min_spacing_ms`
/// after the earliest post-fence one. Every comparison is either within this
/// machine's clock or between event identities — never between two machines'
/// clocks. The earliest post-fence beat stays the anchor, so any later
/// distinct beat clearing the spacing proves liveness.
#[derive(Debug)]
pub struct LiveEvidenceTracker {
    since_ms: u64,
    min_spacing_ms: u64,
    anchor: Option<HeartbeatObservation>,
}

impl LiveEvidenceTracker {
    /// A tracker fenced at `since_ms` with the standard spacing.
    #[must_use]
    pub fn new(since_ms: u64) -> Self {
        Self::with_spacing(since_ms, WAKE_EVIDENCE_MIN_SPACING_MS)
    }

    /// A tracker with an explicit spacing requirement.
    #[must_use]
    pub fn with_spacing(since_ms: u64, min_spacing_ms: u64) -> Self {
        Self {
            since_ms,
            min_spacing_ms,
            anchor: None,
        }
    }

    /// Feed the latest observation. Returns `true` once liveness is proven.
    pub fn observe(&mut self, current: Option<&HeartbeatObservation>) -> bool {
        let Some(current) = current else {
            return false;
        };
        if current.observed_at_ms < self.since_ms {
            return false;
        }
        let Some(anchor) = self.anchor.as_ref() else {
            self.anchor = Some(current.clone());
            return false;
        };
        if current.event_id == anchor.event_id {
            return false;
        }
        current.observed_at_ms >= anchor.observed_at_ms.saturating_add(self.min_spacing_ms)
    }

    /// Has at least one post-fence beat been observed?
    ///
    /// Drives the early bailout: no beat at all within a heartbeat interval
    /// means the `online` status has no process behind it.
    #[must_use]
    pub fn has_post_fence_beat(&self) -> bool {
        self.anchor.is_some()
    }
}

/// Is a provider-backed agent live, per freshly fetched presence?
///
/// Mirrors the desktop's `isManagedAgentLive` for the provider branch — the
/// only branch reachable here, because [`crate::decide::select_wake_candidates`]
/// has already excluded local agents. `None` is a resolved lookup with no
/// record, which is a real observation of "no presence"; a *failed* lookup
/// never reaches this function.
#[must_use]
pub fn is_managed_agent_live(presence: Option<PresenceStatus>) -> bool {
    matches!(
        presence,
        Some(PresenceStatus::Online | PresenceStatus::Away)
    )
}

/// Has this agent been woken recently enough that another attempt is noise?
///
/// A clock that moved backwards counts as debounced: the alternative is
/// treating a bogus future timestamp as permission to deploy on every event.
#[must_use]
pub fn is_wake_attempt_debounced(last_attempt_at_ms: Option<u64>, now_ms: u64) -> bool {
    is_wake_attempt_debounced_within(last_attempt_at_ms, now_ms, WAKE_ATTEMPT_DEBOUNCE_MS)
}

/// [`is_wake_attempt_debounced`] with an explicit window.
#[must_use]
pub fn is_wake_attempt_debounced_within(
    last_attempt_at_ms: Option<u64>,
    now_ms: u64,
    window_ms: u64,
) -> bool {
    let Some(last) = last_attempt_at_ms else {
        return false;
    };
    // Saturating rather than wrapping: `now < last` is the backwards clock,
    // and it saturates to 0, which is inside every window — debounced.
    now_ms.saturating_sub(last) < window_ms
}

/// Should freshly collapsed triggers be re-driven immediately when the owning
/// attempt ends with `outcome`?
///
/// True exactly for the exits where the owner neither proved liveness nor
/// spent a deploy on anyone's behalf. There an immediate re-drive costs
/// nothing it should not, and lets a legitimate follower become the next
/// owner. Every other owned exit retains the follower for the armed timer,
/// because no settlement is positive — see [`is_presumed_delivered_by_floor`].
#[must_use]
pub fn should_retry_collapsed_triggers(outcome: WakeOutcome) -> bool {
    matches!(
        outcome,
        WakeOutcome::AuthorRejected
            | WakeOutcome::AuthorUnverified
            | WakeOutcome::PresenceUnavailable
    )
}

/// Buffer a trigger that cannot be evaluated yet, or that is retained behind
/// an in-flight attempt.
///
/// Deduplicates by event id (the tap can deliver one event via both the broad
/// and the mention subscription) and drops the **oldest** beyond `limit` — the
/// newest mentions are the most actionable, and nothing dropped here can be
/// recovered, because the tap has no history replay.
pub fn push_bounded_pending_trigger<T: HasEventId>(queue: &mut Vec<T>, event: T, limit: usize) {
    if queue
        .iter()
        .any(|queued| queued.event_id() == event.event_id())
    {
        return;
    }
    queue.push(event);
    if queue.len() > limit {
        queue.remove(0);
    }
}

/// The one field [`push_bounded_pending_trigger`] needs, so callers can buffer
/// their own trigger type rather than a translation of it.
pub trait HasEventId {
    /// The event's id, hex.
    fn event_id(&self) -> &str;
}

impl HasEventId for crate::decide::TriggerEvent {
    fn event_id(&self) -> &str {
        &self.id
    }
}

/// How an attempt's deploy settled, from the caller's side.
#[derive(Debug, Clone, Copy)]
pub struct WakeSettlement {
    /// How the attempt ended.
    pub outcome: WakeOutcome,
    /// The replay floor the deploy committed, if one was committed.
    pub committed_floor_ts: Option<u64>,
    /// The **provider's** proof that the deploy started a fresh generation.
    ///
    /// `false` covers both "the provider said no-op" and "the provider gave no
    /// classification" — unproven is unproven.
    pub floor_adopted: bool,
}

/// Should a settled trigger be *presumed* delivered by the deploy its owning
/// attempt committed?
///
/// A presumption, never a proof — it must not authorize dropping a trigger.
/// Its only consumer is the terminal drop log after the one-shot retry, which
/// it downgrades from a warning to an informational line.
///
/// The strongest chain available has exactly one shape: the attempt ended
/// [`WakeOutcome::Woken`], the *provider* proved the deploy started a fresh
/// generation (so the env carrying the committed floor is in effect), and the
/// floor's REQ window reaches the trigger's `created_at`. Even that chain stops
/// at the harness process boundary — in `buzz-acp`, `subscribe_channel` returns
/// once the REQ is *enqueued*, which can then sit rate-gated or be rejected
/// after presence is already published. A floor provably booted is still not
/// the target channel's delivery.
///
/// Liveness alone grounds nothing: heartbeats prove a harness runs somewhere,
/// not that this channel's REQ was active when the mention was delivered.
///
/// Retried triggers are never presumed delivered even by a proven fresh
/// generation: their age since first delivery includes a full failed attempt
/// plus the stranded-retry delay, which can exceed
/// [`crate::WAKE_DELIVERABLE_AGE_SECS`] — a clamped floor silently excludes
/// them, and nothing on this side can check the harness's clock to know.
#[must_use]
pub fn is_presumed_delivered_by_floor(
    trigger_created_at: u64,
    trigger_retried_once: bool,
    settlement: &WakeSettlement,
) -> bool {
    if trigger_retried_once || settlement.outcome != WakeOutcome::Woken || !settlement.floor_adopted
    {
        return false;
    }
    match settlement.committed_floor_ts {
        Some(floor) => crate::decide::is_covered_by_replay_floor(trigger_created_at, floor),
        None => false,
    }
}

/// Per-agent bookkeeping shared across attempts.
///
/// Lives outside [`run_wake_attempt`] because its whole purpose is to be seen
/// by the *other* concurrent attempts: the in-flight claim is what collapses a
/// burst of mentions into one deploy. Shared by `&`, with the map behind a
/// mutex, so concurrently spawned attempts observe one book — the desktop got
/// this for free from a single-threaded event loop and this crate cannot.
#[derive(Debug, Default)]
pub struct WakeAttemptState {
    book: Mutex<AttemptBook>,
}

#[derive(Debug, Default)]
struct AttemptBook {
    last_attempt_at: HashMap<String, u64>,
    in_flight: HashSet<String>,
}

impl WakeAttemptState {
    /// An empty book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the lock, recovering from poisoning rather than propagating it.
    ///
    /// A panic inside a previous attempt leaves the book intact — every write
    /// under this lock is a single map or set operation — and refusing every
    /// future wake because one attempt panicked would turn a transient bug into
    /// a permanently deaf daemon.
    fn book(&self) -> std::sync::MutexGuard<'_, AttemptBook> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Claim the right to decide for this agent, or say why not.
    ///
    /// The three refusals are checked and the claim taken **under one lock**.
    /// Splitting them would reintroduce exactly the race the in-flight set
    /// exists to close: two mentions landing together, both reading an empty
    /// set, both deploying. `aborted` is passed in rather than read here so the
    /// ordering still matches the desktop's — in-flight, then debounce, then
    /// the generation fence — without the check escaping the lock.
    fn claim<'a>(
        &'a self,
        key: &'a str,
        now_ms: u64,
        aborted: bool,
    ) -> Result<AttemptClaim<'a>, WakeOutcome> {
        let mut book = self.book();
        if book.in_flight.contains(key) {
            return Err(WakeOutcome::InFlight);
        }
        if is_wake_attempt_debounced(book.last_attempt_at.get(key).copied(), now_ms) {
            return Err(WakeOutcome::Debounced);
        }
        if aborted {
            return Err(WakeOutcome::Cancelled);
        }
        book.in_flight.insert(key.to_string());
        drop(book);
        Ok(AttemptClaim { state: self, key })
    }

    /// Record that an attempt for this agent has just spent a deploy.
    fn stamp(&self, key: &str, at_ms: u64) {
        self.book().last_attempt_at.insert(key.to_string(), at_ms);
    }

    /// Move our stamp forward to when the deploy actually settled — but
    /// **only if it is still ours**, on the same reasoning as
    /// [`Self::release_stamp`].
    ///
    /// The stamp is taken *before* the provider call, because the attempt is
    /// what the debounce counts. That is only sound while the call is shorter
    /// than the window, and it is not: the deploy deadline is 600s (the
    /// desktop's `invoke_provider`) against a 120s
    /// [`WAKE_ATTEMPT_DEBOUNCE_MS`]. A refusal that arrives after the window
    /// has already lapsed would leave no cooldown at all, and the next mention
    /// would immediately spend another slow, failing deploy — turning a
    /// provider outage into a stream of expensive calls. Re-measuring from
    /// settlement is what makes "hold the debounce after a refusal" actually
    /// hold.
    ///
    /// Never moves the stamp backwards: a clock that jumped back would
    /// otherwise *shorten* the window it is here to guarantee.
    fn refresh_stamp(&self, key: &str, stamped_at: u64, settled_at: u64) {
        let mut book = self.book();
        if book.last_attempt_at.get(key).copied() == Some(stamped_at) {
            book.last_attempt_at
                .insert(key.to_string(), settled_at.max(stamped_at));
        }
    }

    /// Release a stamp, but **only if it is still ours** — a later attempt may
    /// have re-stamped, and clearing its window would let a burst through.
    fn release_stamp(&self, key: &str, stamped_at: u64) {
        let mut book = self.book();
        if book.last_attempt_at.get(key).copied() == Some(stamped_at) {
            book.last_attempt_at.remove(key);
        }
    }

    /// Is an attempt for this agent currently deciding? Test and diagnostic
    /// visibility only — the claim itself is taken inside [`Self::claim`].
    #[must_use]
    pub fn is_in_flight(&self, pubkey: &str) -> bool {
        self.book().in_flight.contains(&normalize_pubkey(pubkey))
    }

    /// When this agent was last deployed by an attempt, if ever.
    #[must_use]
    pub fn last_attempt_at(&self, pubkey: &str) -> Option<u64> {
        self.book()
            .last_attempt_at
            .get(&normalize_pubkey(pubkey))
            .copied()
    }
}

/// Holds the in-flight claim for the life of one attempt.
///
/// A guard rather than a `finally`: the claim must be released on *every* exit
/// including an early `?`, a cancellation, or a panic. Leaking one would make
/// the agent permanently unwakeable, which is the worst failure in this file.
struct AttemptClaim<'a> {
    state: &'a WakeAttemptState,
    key: &'a str,
}

impl Drop for AttemptClaim<'_> {
    fn drop(&mut self) {
        self.state.book().in_flight.remove(self.key);
    }
}

/// Everything a wake attempt does to the outside world.
///
/// Split out so the attempt's *sequencing* — which is the part that has failed
/// in review, repeatedly — can be tested without a relay, a provider or a real
/// two-minute wait. Production wires these to the relay presence query, the
/// heartbeat log, and the provider deploy.
///
/// Every method takes `&self`: an implementation that needs to mutate (a clock,
/// a counter) uses interior mutability, which keeps the effects usable from a
/// spawned task alongside the shared [`WakeAttemptState`].
pub trait WakeEffects {
    /// Errors from the fallible effects.
    type Error;

    /// This machine's clock, milliseconds. Never the relay's, never an
    /// emitter's.
    fn now_ms(&self) -> u64;

    /// Has this attempt's generation fence fired?
    ///
    /// Checked after every wait and before every external effect: an attempt
    /// started under community A must never run its author fetch or its deploy
    /// against community B's workspace after a switch.
    fn is_cancelled(&self) -> bool;

    /// The latest heartbeat delivery observed for this agent, if any.
    ///
    /// `None` means the log holds no entry — which, mid-attempt, is the harness
    /// announcing its own exit.
    fn heartbeat(&self) -> Option<HeartbeatObservation>;

    /// Fires the moment a non-reconcile deploy is accepted, before convergence.
    /// The "waking up" surface belongs here, not on the final outcome.
    fn on_deployed(&self) {}

    /// Sleep. Injected so tests exercise the local-time spacing rules for real
    /// against a fake clock instead of waiting out 135 seconds.
    fn delay(&self, ms: u64) -> impl Future<Output = ()>;

    /// Fetch this agent's presence **fresh**.
    ///
    /// `Ok(None)` is a resolved lookup with no record — a real observation of
    /// "offline". An `Err` is *unknown*, and must never be collapsed into
    /// "offline": that would deploy on every relay hiccup.
    fn presence(&self) -> impl Future<Output = Result<Option<PresenceStatus>, Self::Error>>;

    /// Re-validate the triggering event's author against a **fresh**
    /// known-agent set, immediately before the deploy is spent.
    ///
    /// `Ok(true)` means the author is confirmed *not* to be a known agent. The
    /// synchronous baseline the caller filtered with can be minutes stale, and
    /// an agent registered meanwhile on another desktop must not be able to
    /// wake this one through that window. An implementation with nothing to
    /// re-check returns `Ok(true)`.
    fn confirm_author_not_known_agent(&self) -> impl Future<Output = Result<bool, Self::Error>>;

    /// Deploy the agent. Idempotent by contract: the provider reconciles to
    /// at-most-one instance and treats a live agent as a strict no-op.
    ///
    /// The `Ok` payload is the provider's own fresh-generation classification
    /// (see [`WakeAttemptResult::floor_adopted`]) — `None` if the provider
    /// gave none.
    fn start_managed_agent(&self) -> impl Future<Output = Result<Option<bool>, Self::Error>>;
}

/// Decide and perform one wake.
///
/// The sequence, and the reason for each step:
///
/// 1. **Claim, or bow out** — in-flight, debounce, generation fence.
/// 2. **Sample presence fresh.** Never a cached snapshot: this decision starts
///    a machine.
/// 3. **If the status says live, make it prove it** — two distinct spaced
///    beats, with an early bailout when not even one arrives. Unproven means
///    reconcile through the idempotent deploy; an *announced exit* (the entry
///    disappearing) is a real death and takes the fenced dead path.
/// 4. **Dead path: wait out the teardown fence**, then look once more. The old
///    process can outlive its own `offline` publish, and deploying inside that
///    window no-ops against a corpse.
/// 5. **Re-validate the author**, then and only then.
/// 6. **Stamp, then deploy.** Stamping first is deliberate: the *attempt* is
///    what the debounce counts, and a slow deploy must not let a burst through
///    behind it. A *failed* deploy then re-stamps from settlement, because the
///    provider is allowed to take longer to refuse than the whole window
///    ([`WakeAttemptState::refresh_stamp`]).
/// 7. **Converge on a beat delivered after the deploy completed.** A deploy
///    return alone can be a no-op against a process that was still dying. If
///    nothing appears, release our own stamp so the next mention retries
///    instead of being debounced against a dead agent.
pub async fn run_wake_attempt<E: WakeEffects>(
    agent_pubkey: &str,
    state: &WakeAttemptState,
    effects: &E,
) -> WakeAttemptResult<E::Error> {
    let key = normalize_pubkey(agent_pubkey);
    let attempt_started_at = effects.now_ms();

    // Held for the rest of the function; dropping it releases the claim on
    // every exit path, including a panic.
    let _claim = match state.claim(&key, attempt_started_at, effects.is_cancelled()) {
        Ok(claim) => claim,
        Err(outcome) => return WakeAttemptResult::plain(outcome, false),
    };

    let mut tracker = LiveEvidenceTracker::new(attempt_started_at);

    // An unresolved lookup is *unknown*, and unknown is not a refusal.
    //
    // It is the same epistemic state as an `online` this attempt cannot
    // prove, and step 3 already answers that one by reconciling through the
    // idempotent deploy — which a live agent turns into a strict no-op.
    // Refusing here instead bought nothing that costs, and guaranteed the one
    // thing that does: presence is ephemeral, so an agent that is already
    // down publishes nothing to resolve the lookup with, and
    // `PresenceState::mark_disconnected` reopens the same hole on every
    // reconnect. An agent this daemon has never observed could therefore
    // never be woken — exactly the case remote wake exists for.
    //
    // The error itself is not dropped: `WakeEffects::presence`'s
    // implementation reports it, because this decision core stays free of
    // side effects.
    let mut presence_unknown = false;
    let mut presence = match effects.presence().await {
        Ok(presence) => presence,
        Err(_) => {
            presence_unknown = true;
            None
        }
    };
    if effects.is_cancelled() {
        return WakeAttemptResult::plain(WakeOutcome::Cancelled, false);
    }

    let mut reconcile = false;
    if is_managed_agent_live(presence) {
        // An entry present at the start is what makes its later disappearance
        // meaningful: the harness publishing `offline` clears it. Without a
        // starting entry there is nothing to disappear, and absence is just
        // "no beats reaching us".
        let had_entry_at_start = effects.heartbeat().is_some();
        let mut announced_exit = false;

        for attempt in 0..WAKE_LIVE_EVIDENCE_ATTEMPTS {
            if tracker.observe(effects.heartbeat().as_ref()) {
                return WakeAttemptResult::plain(WakeOutcome::AlreadyLive, false);
            }
            if had_entry_at_start && effects.heartbeat().is_none() {
                announced_exit = true;
                break;
            }
            if attempt >= WAKE_LIVE_NO_BEAT_BAILOUT_ATTEMPTS && !tracker.has_post_fence_beat() {
                break;
            }
            effects.delay(WAKE_LIVE_EVIDENCE_POLL_MS).await;
            if effects.is_cancelled() {
                return WakeAttemptResult::plain(WakeOutcome::Cancelled, false);
            }
        }

        // The loop can exit on its last delay without re-reading, so both
        // terminal conditions are checked once more here.
        if tracker.observe(effects.heartbeat().as_ref()) {
            return WakeAttemptResult::plain(WakeOutcome::AlreadyLive, false);
        }
        if had_entry_at_start && effects.heartbeat().is_none() {
            announced_exit = true;
        }

        // Unproven: the `online` is unverifiable — a crashed harness, a lone
        // delayed final beat, or heartbeats not reaching us — so reconcile
        // through the deploy. An announced exit is a real death instead, and
        // takes the fenced dead path below.
        reconcile = !announced_exit;
    }

    // The teardown fence answers a death this attempt *observed* — the old
    // process outliving its own `offline` publish. Unknown observed no death,
    // so there is no window to wait out, and waiting would only delay a wake
    // for an agent whose state never resolves anyway.
    if !reconcile && !presence_unknown {
        effects.delay(REMOTE_POST_OFFLINE_GRACE_MS).await;
        if effects.is_cancelled() {
            return WakeAttemptResult::plain(WakeOutcome::Cancelled, false);
        }
        // Unknown again, after the fence this time: same rule as the first
        // lookup — unproven liveness reconciles through the deploy rather
        // than refusing, so an unresolved read falls back to "no record". No
        // flag to set; the fence it would have skipped is already waited out.
        presence = effects.presence().await.unwrap_or_default();
        // A fresh generation appearing meanwhile — another client's deploy —
        // can still prove itself through the same tracker, and the fence is
        // one poll interval wide, which is not enough for two spaced beats
        // unless a real process is behind them.
        if tracker.observe(effects.heartbeat().as_ref()) {
            return WakeAttemptResult::plain(WakeOutcome::AlreadyLive, false);
        }
        // Status resurfacing live without proof is the unverifiable case
        // again: deploy, but as reconciliation.
        reconcile = is_managed_agent_live(presence);
    }

    // Generation fence before every external effect: the author fetch and the
    // deploy both act on the *current* workspace.
    if effects.is_cancelled() {
        return WakeAttemptResult::plain(WakeOutcome::Cancelled, reconcile);
    }

    // Last gate before spending money. No stamp on refusal — a legitimate
    // mention right after must not find the window closed by a wake that
    // never happened.
    match effects.confirm_author_not_known_agent().await {
        Ok(true) => {}
        Ok(false) => return WakeAttemptResult::plain(WakeOutcome::AuthorRejected, reconcile),
        Err(error) => {
            return WakeAttemptResult::failed(WakeOutcome::AuthorUnverified, reconcile, error)
        }
    }
    if effects.is_cancelled() {
        return WakeAttemptResult::plain(WakeOutcome::Cancelled, reconcile);
    }

    let stamped_at = effects.now_ms();
    state.stamp(&key, stamped_at);
    let floor_adopted = match effects.start_managed_agent().await {
        Ok(floor_adopted) => floor_adopted,
        Err(error) => {
            // The fence can fire while the provider call is pending — the
            // unmounted generation's error must not surface in its successor.
            if effects.is_cancelled() {
                return WakeAttemptResult::plain(WakeOutcome::Cancelled, reconcile);
            }
            // Holding the debounce after a refusal is deliberate: a provider
            // that just refused will refuse the next mention too. Measured
            // from *settlement*, because a provider call is allowed to
            // outlast the window it is supposed to close (600s deploy
            // deadline, 120s debounce) and a stamp that expired while the
            // call was still pending is not a cooldown at all.
            state.refresh_stamp(&key, stamped_at, effects.now_ms());
            return WakeAttemptResult::failed(WakeOutcome::DeployFailed, reconcile, error);
        }
    };
    if effects.is_cancelled() {
        // Same fence on the success path: the deploy happened under the right
        // generation, but its surface must not appear in the next one.
        return WakeAttemptResult::plain(WakeOutcome::Cancelled, reconcile);
    }
    if !reconcile {
        effects.on_deployed();
    }
    let deployed_at = effects.now_ms();

    // Converge on a beat delivered *after* the deploy completed. That fence is
    // local-clock-only and sits at least one teardown fence (dead path) or a
    // full evidence window (reconcile path) after the attempt began, so an old
    // generation's in-flight beat cannot reach past it; the expected signal is
    // the fresh generation's startup presence publish.
    for _ in 0..WAKE_CONFIRM_ATTEMPTS {
        effects.delay(WAKE_CONFIRM_POLL_MS).await;
        if effects.is_cancelled() {
            // The deploy already happened under the right generation; only the
            // watching stops.
            return WakeAttemptResult::plain(WakeOutcome::Cancelled, reconcile);
        }
        if let Some(observation) = effects.heartbeat() {
            if observation.observed_at_ms >= deployed_at {
                return WakeAttemptResult::woken(reconcile, floor_adopted);
            }
        }
    }

    state.release_stamp(&key, stamped_at);
    WakeAttemptResult::plain(WakeOutcome::WakeUnconfirmed, reconcile)
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    const AGENT: &str = "a1b2c3";
    /// A fixed origin for the fake clock. Any value; nothing depends on it.
    const CLOCK: u64 = 1_700_000_000_000;

    #[derive(Debug, PartialEq, Eq)]
    struct EffectError(&'static str);

    /// The scripted world one attempt runs against.
    ///
    /// Mirrors the desktop test harness: presence statuses are consumed one per
    /// lookup (the last entry repeats, `None` = no record), the clock advances
    /// with every `delay` so the local-time spacing rules are exercised for
    /// real, and heartbeat deliveries are injected by delay count.
    struct Harness {
        presence_script: Vec<Option<PresenceStatus>>,
        script_index: Cell<usize>,
        /// Delay counts after which a new distinct beat is recorded.
        beats_after_delays: Vec<u32>,
        /// Delay count after which the heartbeat entry disappears — the
        /// harness announcing its exit.
        offline_after_delays: Option<u32>,
        /// Delay count after which the generation fence fires.
        cancel_after_delays: Option<u32>,
        cancel_during_deploy: bool,
        evidence_on_deploy: bool,
        deploy_fails: bool,
        /// What `start_managed_agent` reports as the provider's
        /// fresh-generation classification on a successful deploy.
        deploy_fresh_generation: Option<bool>,
        /// How long the provider call occupies the clock. Non-zero exercises
        /// the case the constants do not cover on their own: a deploy allowed
        /// to run five times the debounce window.
        deploy_duration_ms: u64,
        author_is_agent: bool,
        author_check_fails: bool,
        presence_fails: bool,

        clock: Cell<u64>,
        delay_count: Cell<u32>,
        beat_serial: Cell<u32>,
        cancelled: Cell<bool>,
        evidence: RefCell<Option<HeartbeatObservation>>,
        delays: RefCell<Vec<u64>>,
        deploys: Cell<u32>,
        on_deployed_calls: Cell<u32>,
        /// Delay count at the moment the author re-check ran, so ordering
        /// against the deploy is assertable.
        author_checked_at_delay: Cell<Option<u32>>,
        /// Delay count at the moment the deploy was spent. The evidence poll
        /// and the convergence poll are both 5s, so counting delays *by
        /// duration* cannot tell the two phases apart — this splits them.
        delays_before_deploy: Cell<Option<u32>>,
    }

    impl Default for Harness {
        fn default() -> Self {
            Self {
                presence_script: Vec::new(),
                script_index: Cell::new(0),
                beats_after_delays: Vec::new(),
                offline_after_delays: None,
                cancel_after_delays: None,
                cancel_during_deploy: false,
                evidence_on_deploy: true,
                deploy_fails: false,
                deploy_fresh_generation: None,
                deploy_duration_ms: 0,
                author_is_agent: false,
                author_check_fails: false,
                presence_fails: false,
                clock: Cell::new(CLOCK),
                delay_count: Cell::new(0),
                beat_serial: Cell::new(0),
                cancelled: Cell::new(false),
                evidence: RefCell::new(None),
                delays: RefCell::new(Vec::new()),
                deploys: Cell::new(0),
                on_deployed_calls: Cell::new(0),
                author_checked_at_delay: Cell::new(None),
                delays_before_deploy: Cell::new(None),
            }
        }
    }

    impl Harness {
        fn with_presence(mut self, script: &[Option<PresenceStatus>]) -> Self {
            self.presence_script = script.to_vec();
            self
        }

        fn beats_after(mut self, delays: &[u32]) -> Self {
            self.beats_after_delays = delays.to_vec();
            self
        }

        fn seed_heartbeat(self, observed_at_ms: u64) -> Self {
            *self.evidence.borrow_mut() = Some(HeartbeatObservation {
                event_id: "hb-seed".to_string(),
                observed_at_ms,
            });
            self
        }

        fn advance(&self, ms: u64) {
            self.clock.set(self.clock.get() + ms);
        }

        fn record_beat(&self) {
            let serial = self.beat_serial.get() + 1;
            self.beat_serial.set(serial);
            *self.evidence.borrow_mut() = Some(HeartbeatObservation {
                event_id: format!("hb-{serial}"),
                observed_at_ms: self.clock.get(),
            });
        }

        fn delays(&self) -> Vec<u64> {
            self.delays.borrow().clone()
        }
    }

    impl WakeEffects for Harness {
        type Error = EffectError;

        fn now_ms(&self) -> u64 {
            self.clock.get()
        }

        fn is_cancelled(&self) -> bool {
            self.cancelled.get()
        }

        fn heartbeat(&self) -> Option<HeartbeatObservation> {
            self.evidence.borrow().clone()
        }

        fn on_deployed(&self) {
            self.on_deployed_calls.set(self.on_deployed_calls.get() + 1);
        }

        async fn delay(&self, ms: u64) {
            self.delays.borrow_mut().push(ms);
            self.advance(ms);
            let count = self.delay_count.get() + 1;
            self.delay_count.set(count);
            if self.beats_after_delays.contains(&count) {
                self.record_beat();
            }
            if self.offline_after_delays == Some(count) {
                *self.evidence.borrow_mut() = None;
            }
            if self.cancel_after_delays == Some(count) {
                self.cancelled.set(true);
            }
            // A real suspension point. Without one, an attempt whose effects
            // are all instantly-ready futures runs to completion on its first
            // poll, and two concurrently spawned attempts would never actually
            // overlap — which is precisely what the in-flight claim exists to
            // handle.
            tokio::task::yield_now().await;
        }

        async fn presence(&self) -> Result<Option<PresenceStatus>, Self::Error> {
            if self.presence_fails {
                return Err(EffectError("relay unavailable"));
            }
            let index = self.script_index.get();
            let status = if index < self.presence_script.len() {
                self.script_index.set(index + 1);
                self.presence_script[index]
            } else {
                self.presence_script.last().copied().flatten()
            };
            Ok(status)
        }

        async fn confirm_author_not_known_agent(&self) -> Result<bool, Self::Error> {
            self.author_checked_at_delay
                .set(Some(self.delay_count.get()));
            if self.author_check_fails {
                return Err(EffectError("known-agent lookup failed"));
            }
            Ok(!self.author_is_agent)
        }

        async fn start_managed_agent(&self) -> Result<Option<bool>, Self::Error> {
            self.deploys.set(self.deploys.get() + 1);
            self.delays_before_deploy.set(Some(self.delay_count.get()));
            // Time the provider spends before answering. Not a `delay`: it
            // moves the clock without touching the delay bookkeeping the
            // phase assertions read.
            self.advance(self.deploy_duration_ms);
            if self.cancel_during_deploy {
                self.cancelled.set(true);
            }
            if self.deploy_fails {
                return Err(EffectError("provider refused"));
            }
            if self.evidence_on_deploy {
                self.record_beat();
            }
            Ok(self.deploy_fresh_generation)
        }
    }

    async fn attempt(
        harness: &Harness,
        state: &WakeAttemptState,
    ) -> WakeAttemptResult<EffectError> {
        run_wake_attempt(AGENT, state, harness).await
    }

    #[tokio::test]
    async fn an_offline_agent_is_deployed_exactly_once() {
        let harness = Harness::default();
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::Woken);
        assert_eq!(harness.deploys.get(), 1);
        assert!(
            harness.delays().contains(&REMOTE_POST_OFFLINE_GRACE_MS),
            "the deploy must respect the post-offline teardown fence"
        );
    }

    #[tokio::test]
    async fn a_provider_proven_fresh_deploy_reports_floor_adopted() {
        let harness = Harness {
            deploy_fresh_generation: Some(true),
            ..Harness::default()
        };
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::Woken);
        assert_eq!(result.floor_adopted, Some(true));
    }

    #[tokio::test]
    async fn a_strict_no_op_deploy_reports_the_floor_was_not_adopted_even_though_woken() {
        // A heartbeat still arrives — from the already-running generation —
        // so the outcome is Woken, but the provider proved this deploy's env
        // (the replay floor included) never took effect. A caller must not
        // read `Woken` alone as proof the mention was delivered.
        let harness = Harness {
            deploy_fresh_generation: Some(false),
            ..Harness::default()
        };
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::Woken);
        assert_eq!(result.floor_adopted, Some(false));
    }

    #[tokio::test]
    async fn a_provider_giving_no_classification_reports_the_floor_as_unproven() {
        let harness = Harness::default();
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::Woken);
        assert_eq!(
            result.floor_adopted, None,
            "a provider predating the classification must read as unproven, not adopted"
        );
    }

    #[tokio::test]
    async fn a_live_agent_that_keeps_heartbeating_is_left_alone() {
        // The only accepted proof: two distinct beats delivered after the
        // attempt began, spaced in local time. T+5s and T+40s.
        let harness = Harness::default()
            .with_presence(&[Some(PresenceStatus::Online)])
            .beats_after(&[1, 8]);
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::AlreadyLive);
        assert_eq!(harness.deploys.get(), 0);
    }

    #[tokio::test]
    async fn a_dying_harness_that_still_says_online_is_woken_once_it_announces_exit() {
        // The mention races a harness whose main loop already chose shutdown.
        // Its offline publish clears the heartbeat entry mid-wait, which routes
        // to the dead path — teardown fence, then a real deploy.
        let harness = Harness::default()
            .with_presence(&[Some(PresenceStatus::Online), Some(PresenceStatus::Offline)])
            .seed_heartbeat(CLOCK - 1_000);
        let harness = Harness {
            offline_after_delays: Some(2),
            ..harness
        };
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::Woken);
        assert!(!result.reconcile, "an announced exit is a real death");
        assert_eq!(harness.deploys.get(), 1);
    }

    #[tokio::test]
    async fn a_pre_attempt_heartbeat_is_not_proof_of_life() {
        // The crash window: a beat delivered one second before the attempt
        // began says nothing about the second after it. Nothing lands post
        // fence, so the attempt reconciles through the deploy.
        let harness = Harness::default()
            .with_presence(&[Some(PresenceStatus::Online)])
            .seed_heartbeat(CLOCK - 1_000);
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::Woken);
        assert!(result.reconcile, "the status still claimed online");
        assert_eq!(harness.deploys.get(), 1);
        assert_eq!(
            harness.on_deployed_calls.get(),
            0,
            "a reconcile deploy must not raise the waking-up surface"
        );
    }

    #[tokio::test]
    async fn a_lone_delayed_final_heartbeat_is_not_proof_of_life() {
        // A dying generation's last in-flight beat lands after the fence. One
        // beat is not two, so it proves nothing and the deploy proceeds.
        let harness = Harness::default()
            .with_presence(&[Some(PresenceStatus::Online)])
            .beats_after(&[1]);
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::Woken);
        assert!(result.reconcile);
        assert_eq!(harness.deploys.get(), 1);
    }

    #[tokio::test]
    async fn two_beats_delivered_too_close_together_are_not_proof_of_life() {
        // Distinct beats, but 5s apart — inside WAKE_EVIDENCE_MIN_SPACING_MS.
        // A dying process flushing a queue could produce that; a live one
        // cannot avoid producing a properly spaced pair.
        let harness = Harness::default()
            .with_presence(&[Some(PresenceStatus::Online)])
            .beats_after(&[1, 2]);
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::Woken);
        assert!(result.reconcile);
    }

    #[tokio::test]
    async fn the_early_bailout_fires_when_no_beat_arrives_at_all() {
        // An "online" with a heartbeat log that never produces a post-fence
        // beat is a crashed harness's residue. The attempt must not burn the
        // full evidence window before reconciling.
        let harness = Harness::default().with_presence(&[Some(PresenceStatus::Online)]);
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::Woken);
        // Reconcile skips the teardown fence, so every delay before the deploy
        // is an evidence poll.
        assert_eq!(
            harness.delays_before_deploy.get(),
            Some(WAKE_LIVE_NO_BEAT_BAILOUT_ATTEMPTS),
            "the bailout must cut the wait short of the full window"
        );
    }

    #[tokio::test]
    async fn an_unconfirmed_deploy_releases_the_debounce() {
        // No evidence follows the deploy, so the agent is presumed still dead
        // and the next mention must be allowed to try again rather than be
        // debounced against a corpse.
        let harness = Harness {
            evidence_on_deploy: false,
            ..Harness::default()
        };
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::WakeUnconfirmed);
        assert_eq!(state.last_attempt_at(AGENT), None);
    }

    #[tokio::test]
    async fn a_failed_deploy_reports_the_error_and_still_holds_the_debounce() {
        // A provider that just refused will refuse the next mention too.
        let harness = Harness {
            deploy_fails: true,
            ..Harness::default()
        };
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::DeployFailed);
        assert_eq!(result.error, Some(EffectError("provider refused")));
        assert!(state.last_attempt_at(AGENT).is_some());
    }

    #[tokio::test]
    async fn a_deploy_that_fails_slower_than_the_debounce_window_still_holds_it() {
        // The provider's deploy deadline is 600s against a 120s window, so a
        // refusal can settle with the pre-call stamp already expired. Cooling
        // down from settlement is what keeps a provider outage from becoming a
        // stream of long, failing deploys — one per fresh mention.
        let harness = Harness {
            deploy_fails: true,
            deploy_duration_ms: WAKE_ATTEMPT_DEBOUNCE_MS * 2,
            ..Harness::default()
        };
        let state = WakeAttemptState::new();

        let first = attempt(&harness, &state).await;
        assert_eq!(first.outcome, WakeOutcome::DeployFailed);
        // The failure path runs no convergence polls, so the clock has not
        // moved since the provider answered.
        let settled_at = harness.clock.get();
        assert_eq!(
            state.last_attempt_at(AGENT),
            Some(settled_at),
            "the failure cooldown is measured from settlement, not from the pre-call stamp"
        );

        let second = attempt(&harness, &state).await;

        assert_eq!(second.outcome, WakeOutcome::Debounced);
        assert_eq!(
            harness.deploys.get(),
            1,
            "the next mention must not spend a second deploy against a refusing provider"
        );
    }

    #[test]
    fn a_settlement_stamp_never_moves_the_window_backwards() {
        // A clock that jumped back while the provider was answering must not
        // shorten the window the refusal is there to guarantee.
        let state = WakeAttemptState::new();
        state.stamp(AGENT, CLOCK);
        state.refresh_stamp(AGENT, CLOCK, CLOCK - 60_000);

        assert_eq!(state.last_attempt_at(AGENT), Some(CLOCK));
    }

    #[test]
    fn a_settlement_stamp_belonging_to_an_earlier_attempt_is_left_alone() {
        // Same reasoning as release_stamp: clearing or moving a *successor's*
        // window would let a burst through behind this attempt's failure.
        let state = WakeAttemptState::new();
        state.stamp(AGENT, CLOCK + 1_000);
        state.refresh_stamp(AGENT, CLOCK, CLOCK + 500_000);

        assert_eq!(state.last_attempt_at(AGENT), Some(CLOCK + 1_000));
    }

    /// An unresolved lookup is unknown, not offline — and not a refusal.
    ///
    /// This previously returned `PresenceUnavailable` and deployed nothing,
    /// on the reasoning that an outage must not deploy on every hiccup. The
    /// cost of that turned out to be the feature: presence is ephemeral, so a
    /// down agent publishes nothing to resolve the lookup with, and
    /// `mark_disconnected` reopens the gap on every reconnect — an agent the
    /// daemon has never observed could never be woken, which is the case
    /// remote wake exists for. Unknown is the same unproven liveness an
    /// unverifiable `online` already reconciles through the idempotent
    /// deploy, and a live agent turns that deploy into a strict no-op.
    #[tokio::test]
    async fn an_unresolved_presence_lookup_reconciles_through_the_deploy() {
        let harness = Harness {
            presence_fails: true,
            ..Harness::default()
        };
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(harness.deploys.get(), 1, "unknown must not block the wake");
        assert_ne!(
            result.outcome,
            WakeOutcome::PresenceUnavailable,
            "an unresolved lookup is no longer terminal"
        );
        assert!(
            !result.reconcile,
            "nothing claimed `online`, so the waking-up surface must still show"
        );
    }

    #[tokio::test]
    async fn a_resolved_lookup_with_no_entry_means_offline_and_wakes() {
        let harness = Harness::default().with_presence(&[None]);
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::Woken);
        assert!(!result.reconcile);
        assert_eq!(harness.on_deployed_calls.get(), 1);
    }

    #[tokio::test]
    async fn an_author_flagged_by_the_fresh_recheck_never_spends_the_deploy() {
        let harness = Harness {
            author_is_agent: true,
            ..Harness::default()
        };
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::AuthorRejected);
        assert_eq!(harness.deploys.get(), 0);
        assert_eq!(
            state.last_attempt_at(AGENT),
            None,
            "a refusal must not close the window on a legitimate mention"
        );
    }

    #[tokio::test]
    async fn an_unverifiable_author_fails_closed_without_deploying() {
        let harness = Harness {
            author_check_fails: true,
            ..Harness::default()
        };
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::AuthorUnverified);
        assert_eq!(harness.deploys.get(), 0);
        assert_eq!(state.last_attempt_at(AGENT), None);
    }

    #[tokio::test]
    async fn the_author_recheck_runs_only_when_a_deploy_is_imminent() {
        // Not at the top of the attempt: an agent proven live never reaches
        // the deploy, and a known-agent fetch per mention would be pure cost.
        let harness = Harness::default()
            .with_presence(&[Some(PresenceStatus::Online)])
            .beats_after(&[1, 8]);
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::AlreadyLive);
        assert_eq!(harness.author_checked_at_delay.get(), None);
    }

    #[tokio::test]
    async fn a_pre_cancelled_attempt_refuses_immediately() {
        let harness = Harness::default();
        harness.cancelled.set(true);
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::Cancelled);
        assert_eq!(harness.deploys.get(), 0);
        assert!(harness.delays().is_empty());
    }

    #[tokio::test]
    async fn a_cancellation_during_the_teardown_fence_never_reaches_the_deploy() {
        let harness = Harness {
            cancel_after_delays: Some(1),
            ..Harness::default()
        };
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::Cancelled);
        assert_eq!(harness.deploys.get(), 0);
    }

    #[tokio::test]
    async fn a_cancellation_while_the_deploy_settles_suppresses_its_outcome() {
        // The deploy happened under the right generation; its surface must not
        // appear in the successor's.
        let harness = Harness {
            cancel_during_deploy: true,
            ..Harness::default()
        };
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::Cancelled);
        assert_eq!(harness.deploys.get(), 1);
        assert_eq!(harness.on_deployed_calls.get(), 0);
    }

    #[tokio::test]
    async fn a_cancellation_during_convergence_stops_the_watch_after_the_deploy() {
        // Delay 1 is the teardown fence, delay 2 the first convergence poll.
        let harness = Harness {
            cancel_after_delays: Some(2),
            evidence_on_deploy: false,
            ..Harness::default()
        };
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::Cancelled);
        assert_eq!(harness.deploys.get(), 1);
    }

    #[tokio::test]
    async fn a_single_beat_during_the_teardown_fence_does_not_fake_proof() {
        // One beat inside the 10s fence cannot be two spaced ones, so the
        // deploy proceeds.
        let harness = Harness::default().with_presence(&[None]).beats_after(&[1]);
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::Woken);
        assert_eq!(harness.deploys.get(), 1);
    }

    #[tokio::test]
    async fn a_burst_of_mentions_produces_one_deploy_not_one_per_mention() {
        let harness = Harness::default();
        let state = WakeAttemptState::new();

        let first = attempt(&harness, &state).await;
        let second = attempt(&harness, &state).await;
        let third = attempt(&harness, &state).await;

        assert_eq!(first.outcome, WakeOutcome::Woken);
        assert_eq!(second.outcome, WakeOutcome::Debounced);
        assert_eq!(third.outcome, WakeOutcome::Debounced);
        assert_eq!(harness.deploys.get(), 1);
    }

    #[tokio::test]
    async fn two_mentions_landing_together_deploy_once_not_twice() {
        // The in-flight claim, not the debounce: the second attempt starts
        // while the first is still deciding, before any stamp exists.
        let harness = Harness::default();
        let state = WakeAttemptState::new();

        let (first, second) = tokio::join!(attempt(&harness, &state), attempt(&harness, &state));

        let outcomes = [first.outcome, second.outcome];
        assert!(
            outcomes.contains(&WakeOutcome::Woken) && outcomes.contains(&WakeOutcome::InFlight),
            "expected one owner and one collapsed follower, got {outcomes:?}"
        );
        assert_eq!(harness.deploys.get(), 1);
    }

    #[tokio::test]
    async fn the_claim_is_released_on_every_exit() {
        let harness = Harness {
            deploy_fails: true,
            ..Harness::default()
        };
        let state = WakeAttemptState::new();

        let result = attempt(&harness, &state).await;

        assert_eq!(result.outcome, WakeOutcome::DeployFailed);
        assert!(
            !state.is_in_flight(AGENT),
            "a leaked claim makes the agent permanently unwakeable"
        );
    }

    #[tokio::test]
    async fn the_agent_is_wakeable_again_once_the_debounce_window_passes() {
        let harness = Harness::default();
        let state = WakeAttemptState::new();

        let first = attempt(&harness, &state).await;
        assert_eq!(first.outcome, WakeOutcome::Woken);

        harness.advance(WAKE_ATTEMPT_DEBOUNCE_MS);
        let second = attempt(&harness, &state).await;

        assert_eq!(second.outcome, WakeOutcome::Woken);
        assert_eq!(harness.deploys.get(), 2);
    }

    #[tokio::test]
    async fn one_agents_debounce_does_not_silence_another() {
        let harness = Harness::default();
        let state = WakeAttemptState::new();

        let first = run_wake_attempt(AGENT, &state, &harness).await;
        let second = run_wake_attempt("deadbeef", &state, &harness).await;

        assert_eq!(first.outcome, WakeOutcome::Woken);
        assert_eq!(second.outcome, WakeOutcome::Woken);
    }

    #[tokio::test]
    async fn the_debounce_key_is_the_normalized_pubkey() {
        // A mention carrying an upper-case pubkey must not open a second
        // deploy window for the same agent.
        let harness = Harness::default();
        let state = WakeAttemptState::new();

        let first = run_wake_attempt(AGENT, &state, &harness).await;
        let second = run_wake_attempt(&AGENT.to_uppercase(), &state, &harness).await;

        assert_eq!(first.outcome, WakeOutcome::Woken);
        assert_eq!(second.outcome, WakeOutcome::Debounced);
    }

    #[test]
    fn proof_of_life_requires_two_distinct_spaced_deliveries() {
        let mut tracker = LiveEvidenceTracker::new(CLOCK);
        let beat = |id: &str, at: u64| HeartbeatObservation {
            event_id: id.to_string(),
            observed_at_ms: at,
        };

        assert!(!tracker.observe(None), "no observation proves nothing");
        assert!(
            !tracker.observe(Some(&beat("hb-old", CLOCK - 1))),
            "a pre-fence delivery proves nothing"
        );
        assert!(
            !tracker.observe(Some(&beat("hb-1", CLOCK + 1_000))),
            "one beat proves nothing"
        );
        assert!(
            !tracker.observe(Some(&beat("hb-1", CLOCK + 60_000))),
            "the same beat re-read proves nothing"
        );
        assert!(
            !tracker.observe(Some(&beat("hb-2", CLOCK + 20_000))),
            "a second beat too close to the anchor proves nothing"
        );
        assert!(
            tracker.observe(Some(&beat("hb-2", CLOCK + 31_000))),
            "two distinct beats, spaced, are proof"
        );
    }

    #[test]
    fn the_evidence_anchor_is_the_earliest_post_fence_beat() {
        // The anchor must not advance with each observation: if it did, a
        // steady 5s poll stream could never clear the 30s spacing and a live
        // agent would be redeployed every time.
        let mut tracker = LiveEvidenceTracker::new(CLOCK);
        assert!(!tracker.has_post_fence_beat());
        assert!(!tracker.observe(Some(&HeartbeatObservation {
            event_id: "hb-1".to_string(),
            observed_at_ms: CLOCK,
        })));
        assert!(tracker.has_post_fence_beat());
        assert!(!tracker.observe(Some(&HeartbeatObservation {
            event_id: "hb-2".to_string(),
            observed_at_ms: CLOCK + 10_000,
        })));
        assert!(
            tracker.observe(Some(&HeartbeatObservation {
                event_id: "hb-3".to_string(),
                observed_at_ms: CLOCK + WAKE_EVIDENCE_MIN_SPACING_MS,
            })),
            "spacing is measured from the first beat, not the last"
        );
    }

    #[test]
    fn a_backwards_clock_counts_as_debounced() {
        // Treating a bogus future stamp as permission would deploy on every
        // event until the clock caught up.
        assert!(is_wake_attempt_debounced(Some(CLOCK + 60_000), CLOCK));
        assert!(!is_wake_attempt_debounced(None, CLOCK));
    }

    #[test]
    fn the_debounce_outlasts_a_cold_start() {
        assert!(is_wake_attempt_debounced(
            Some(CLOCK),
            CLOCK + WAKE_ATTEMPT_DEBOUNCE_MS - 1
        ));
        assert!(!is_wake_attempt_debounced(
            Some(CLOCK),
            CLOCK + WAKE_ATTEMPT_DEBOUNCE_MS
        ));
    }

    #[test]
    fn collapsed_triggers_retry_only_after_uncovered_exits() {
        for outcome in [
            WakeOutcome::AuthorRejected,
            WakeOutcome::AuthorUnverified,
            WakeOutcome::PresenceUnavailable,
        ] {
            assert!(
                should_retry_collapsed_triggers(outcome),
                "{outcome} spent no deploy and proved no liveness"
            );
        }
        for outcome in [
            WakeOutcome::Woken,
            WakeOutcome::AlreadyLive,
            WakeOutcome::Debounced,
            WakeOutcome::InFlight,
            WakeOutcome::DeployFailed,
            WakeOutcome::WakeUnconfirmed,
            WakeOutcome::Cancelled,
        ] {
            assert!(
                !should_retry_collapsed_triggers(outcome),
                "{outcome} must retain its follower for the armed timer instead"
            );
        }
    }

    #[test]
    fn pending_triggers_are_bounded_and_deduplicated() {
        struct Trigger(&'static str);
        impl HasEventId for Trigger {
            fn event_id(&self) -> &str {
                self.0
            }
        }

        let mut queue: Vec<Trigger> = Vec::new();
        push_bounded_pending_trigger(&mut queue, Trigger("a"), 2);
        push_bounded_pending_trigger(&mut queue, Trigger("a"), 2);
        assert_eq!(queue.len(), 1, "the same event id must not queue twice");

        push_bounded_pending_trigger(&mut queue, Trigger("b"), 2);
        push_bounded_pending_trigger(&mut queue, Trigger("c"), 2);
        assert_eq!(
            queue.iter().map(|t| t.0).collect::<Vec<_>>(),
            vec!["b", "c"],
            "the oldest is dropped — the newest mentions are the actionable ones"
        );
    }

    #[test]
    fn only_a_provider_proven_fresh_generation_grounds_the_delivered_presumption() {
        let settled = |outcome, floor_adopted, committed_floor_ts| WakeSettlement {
            outcome,
            committed_floor_ts,
            floor_adopted,
        };

        assert!(is_presumed_delivered_by_floor(
            1_000,
            false,
            &settled(WakeOutcome::Woken, true, Some(1_000))
        ));
        assert!(
            !is_presumed_delivered_by_floor(
                1_000,
                true,
                &settled(WakeOutcome::Woken, true, Some(1_000))
            ),
            "a retried trigger's age can exceed the harness's floor cap"
        );
        assert!(
            !is_presumed_delivered_by_floor(
                1_000,
                false,
                &settled(WakeOutcome::Woken, false, Some(1_000))
            ),
            "without the provider's fresh-generation proof the floor may never have been adopted"
        );
        assert!(
            !is_presumed_delivered_by_floor(
                1_000,
                false,
                &settled(WakeOutcome::AlreadyLive, true, Some(1_000))
            ),
            "liveness proves a harness runs, not that this channel's REQ was active"
        );
        assert!(
            !is_presumed_delivered_by_floor(1_000, false, &settled(WakeOutcome::Woken, true, None)),
            "no committed floor, no presumption"
        );
        assert!(
            !is_presumed_delivered_by_floor(
                1_000,
                false,
                &settled(WakeOutcome::Woken, true, Some(2_000))
            ),
            "a floor that does not reach the trigger does not cover it"
        );
    }

    #[test]
    fn every_outcome_has_the_desktops_string_form() {
        // The two implementations are compared by these names in logs; a
        // silent rename on one side would make the comparison meaningless.
        assert_eq!(WakeOutcome::Woken.as_str(), "woken");
        assert_eq!(WakeOutcome::AlreadyLive.as_str(), "already-live");
        assert_eq!(WakeOutcome::Debounced.as_str(), "debounced");
        assert_eq!(WakeOutcome::InFlight.as_str(), "in-flight");
        assert_eq!(
            WakeOutcome::PresenceUnavailable.as_str(),
            "presence-unavailable"
        );
        assert_eq!(WakeOutcome::DeployFailed.as_str(), "deploy-failed");
        assert_eq!(WakeOutcome::WakeUnconfirmed.as_str(), "wake-unconfirmed");
        assert_eq!(WakeOutcome::AuthorRejected.as_str(), "author-rejected");
        assert_eq!(WakeOutcome::AuthorUnverified.as_str(), "author-unverified");
        assert_eq!(WakeOutcome::Cancelled.as_str(), "cancelled");
    }

    #[test]
    fn only_online_and_away_count_as_live() {
        assert!(is_managed_agent_live(Some(PresenceStatus::Online)));
        assert!(is_managed_agent_live(Some(PresenceStatus::Away)));
        assert!(!is_managed_agent_live(Some(PresenceStatus::Offline)));
        assert!(
            !is_managed_agent_live(None),
            "a resolved lookup with no record is a real observation of offline"
        );
    }
}
