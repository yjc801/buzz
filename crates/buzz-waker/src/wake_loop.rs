//! The connection loop — build order step 5 ("Operate B") of
//! `PLANS/BUZZ_WAKER_DESIGN.md`.
//!
//! Drives [`crate::feed::FeedTransport`] plus the reconnect ladder
//! ([`crate::feed::reconnect_delay_ms`]) plus [`crate::feed::step`] for one
//! watched agent's mention feed. One loop instance watches one agent;
//! `main.rs` runs one per configured agent, alongside a
//! [`crate::presence_feed::run_presence_tap`] for the same agent.
//!
//! # Why each admitted trigger gets its own task
//!
//! A wake attempt's liveness proof can run past 100s
//! ([`crate::attempt::WAKE_LIVE_EVIDENCE_ATTEMPTS`] ×
//! [`crate::attempt::WAKE_LIVE_EVIDENCE_POLL_MS`] = 135s) and a deploy
//! convergence wait for another two minutes. The relay's own liveness check
//! is a single wall-clock deadline per [`crate::feed::FeedTransport::next_frame`]
//! call — nothing answers a control frame unless something is actively
//! calling it. If this loop ran a trigger's attempt inline, it would stop
//! calling `next_frame` for the length of the attempt, and a perfectly
//! healthy connection would look idle-then-dead from here while the relay's
//! own pings went unanswered by the lower layer. So every admitted trigger is
//! [`tokio::task::JoinSet::spawn`]ed onto its own task, and this loop keeps
//! calling `next_frame` throughout — which is also what keeps the connection
//! answering the relay's heartbeat pings during the wait
//! (`NostrWsConnection::recv_one` only runs while something is polling it).
//!
//! # Cursor ownership
//!
//! [`CursorStore`] is the sole owner of its file for its lifetime (see its
//! module note) and is not `Clone`, so a spawned attempt cannot call
//! [`CursorStore::complete`] or [`CursorStore::abandon`] itself. Attempts
//! report their outcome back through the same [`tokio::task::JoinSet`] that
//! spawned them, and this loop — the cursor's only owner — applies the
//! resulting completion or abandonment serially, interleaved with incoming
//! frames in one `select!`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nostr::{Keys, Tag};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::attempt::{
    run_wake_attempt, ClaimingTrigger, WakeAttemptState, WakeOutcome, WAKE_STRANDED_RETRY_DELAY_MS,
};
use crate::cursor::{Admission, CursorStore, Resume, DEFAULT_COMPLETED_RING};
use crate::decide::{normalize_pubkey, TriggerEvent};
use crate::effects::RealWakeEffects;
use crate::feed::{
    is_channel_access_denied, is_rate_limited, parse_rate_limit_retry_secs, rate_limit_park_ms,
    reconnect_delay_ms, step, FeedStep, FeedTransport, WakeReplay, FEED_IDLE_TIMEOUT_SECS,
};
use crate::presence_feed::PresenceState;
use crate::relay_feed::RelayFeed;
use crate::watch_list::WatchList;

/// Configuration for one agent's wake loop.
#[derive(Clone)]
pub struct WakeLoopConfig {
    /// The relay this agent's mention feed connects to.
    pub relay_url: String,
    /// The agent's own keys — the mention feed authenticates **as this
    /// agent**, per §4 of the design (option (ii)); see `crate::feed`'s
    /// module doc for why one feed cannot watch more than one agent.
    pub keys: Keys,
    /// The NIP-OA attestation tag, if this identity has one.
    pub auth_tag: Option<Tag>,
    /// Where this agent's durable cursor lives on disk. One file per agent —
    /// never shared, because the cursor's admission state is per identity.
    pub cursor_path: PathBuf,
    /// The presence tap shared with every wake attempt for this agent.
    pub presence_state: Arc<PresenceState>,
    /// This daemon's live watch list — see `effects`'s module doc for why
    /// this is the accepted `confirm_author_not_known_agent` baseline, and
    /// `crate::watch_list`'s own doc for why it is read live rather than
    /// snapshotted.
    pub watch_list: WatchList,
    /// The live cache [`crate::bundle_feed::run_bundle_tap`] writes this
    /// agent's admitted bundle into. Read fresh at the moment each attempt is
    /// spawned (never captured once at loop-construction time) so a reissue
    /// admitted mid-run takes effect on the very next wake, with no daemon
    /// restart required.
    pub bundle_state: Arc<crate::bundle_feed::BundleState>,
    /// This agent's own provider credential's environment overlay, if it has
    /// one — see `effects::RealWakeEffects`'s own doc for what this is and
    /// why it's `None` for a statically configured agent.
    pub provider_env: Option<Arc<HashMap<String, String>>>,
    /// How long one mention-feed connection lives before the loop reconnects
    /// on purpose — [`CHANNEL_REFRESH_INTERVAL`] in production.
    pub channel_refresh: Duration,
    /// How long a refresh's reconnect may take, from connect until its
    /// backlog replay completes — [`REFRESH_SETUP_DEADLINE`] in production.
    pub refresh_setup_deadline: Duration,
}

/// How often the mention feed reconnects to re-run channel discovery.
///
/// Between reconnects, a channel the agent joins reaches the live watch only
/// through a member-added notification; when one is missed, every mention in
/// that channel is silently dropped until something else reconnects the feed
/// (seen live: a new issue room watched by nobody for days). The reconnect is
/// the existing, coverage-safe path — it rediscovers every channel and
/// backfills mentions from [`crate::RECONNECT_OVERLAP_SECS`] before the
/// checkpoint, across all channels.
///
/// The timer only *starts* recovery: connecting, discovery, the archive probe,
/// the subscriptions and the backlog replay all follow it, and the missed
/// mention is admitted only during that replay. So the interval is what is
/// left of [`buzz_core::relay::WAKE_RECOVERY_DELAY_BUDGET_SECS`] once the
/// enforced [`REFRESH_SETUP_DEADLINE`] is paid for: a successful refresh admits
/// a missed mention at most the whole budget after it arrived — the term the
/// replay floor, the deliverable age and the reconnect overlap all carry.
pub const CHANNEL_REFRESH_INTERVAL: Duration = Duration::from_secs(
    buzz_core::relay::WAKE_RECOVERY_DELAY_BUDGET_SECS - REFRESH_SETUP_DEADLINE.as_secs(),
);

/// How long a refresh's reconnect may take, from connect until its backlog
/// replay completes. Enforced: a refresh that overruns it is a failed
/// connection and goes through the ordinary reconnect ladder, so this is a
/// real bound on the recovery path rather than an estimate of it.
///
/// Refresh reconnects only. A reconnect after a failure, or at startup, may
/// be replaying a long outage — that is outside the recovery budget by
/// definition (and reported as such by [`Resume::GapTooOld`]), and holding it
/// to this deadline could keep it from ever finishing.
pub const REFRESH_SETUP_DEADLINE: Duration = Duration::from_secs(120);

const _: () = assert!(
    CHANNEL_REFRESH_INTERVAL.as_secs() + REFRESH_SETUP_DEADLINE.as_secs()
        == buzz_core::relay::WAKE_RECOVERY_DELAY_BUDGET_SECS,
    "a refresh's timer and setup deadline together must be exactly the recovery budget"
);

/// Why a setup step bounded by a refresh's deadline did not complete.
enum SetupError<E> {
    /// The step itself failed.
    Step(E),
    /// The refresh's setup deadline passed first.
    Overran,
}

impl<E: std::fmt::Display> std::fmt::Display for SetupError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Step(error) => error.fmt(f),
            Self::Overran => f.write_str("the refresh's setup deadline passed"),
        }
    }
}

/// Await one setup step, bounded by `deadline` when the reconnect has one.
async fn within<T, E>(
    deadline: Option<tokio::time::Instant>,
    step: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, SetupError<E>> {
    match deadline {
        Some(deadline) => match tokio::time::timeout_at(deadline, step).await {
            Ok(result) => result.map_err(SetupError::Step),
            Err(_) => Err(SetupError::Overran),
        },
        None => step.await.map_err(SetupError::Step),
    }
}

/// Sleep until `deadline`, or forever when there is none.
async fn sleep_until_some(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Channels `discovered` holds that `previous` did not: each one is a channel
/// the live watch was missing until this discovery, sorted so the log is
/// reproducible.
fn channels_missed(previous: &HashSet<Uuid>, discovered: &HashSet<Uuid>) -> Vec<Uuid> {
    let mut missed: Vec<Uuid> = discovered.difference(previous).copied().collect();
    missed.sort_unstable();
    missed
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What a finished wake attempt reports back to the loop that spawned it.
struct AttemptReport {
    /// The trigger this attempt ran for, returned so the loop can re-drive it
    /// without having kept a parallel copy keyed by id.
    event: TriggerEvent,
    outcome: WakeOutcome,
    /// Whether this attempt *was* the one re-drive a stranded trigger gets.
    /// A re-driven attempt's settlement is terminal whatever it reports —
    /// see [`Settlement`].
    retried: bool,
    /// For [`WakeOutcome::InFlight`] only: the trigger whose attempt refused
    /// this one. Carried through so [`leaves_mention_unacted`] can check the
    /// collapse for real instead of assuming it — see
    /// [`in_flight_owner_covers`].
    in_flight_owner: Option<ClaimingTrigger>,
}

/// Debounce-refused (and otherwise unresolved) triggers retained per agent for
/// their one re-drive, at most.
///
/// Matches the desktop's `WAKE_COLLAPSED_TRIGGER_LIMIT`. One wake loop watches
/// exactly one agent, so this is already the per-agent bound the desktop
/// computes per key. Beyond it the OLDEST — by the mention's own `created_at`,
/// not by arrival order; see [`retain_stranded`] — is dropped, because newest
/// mentions are the most actionable, and dropping is not silent: the cursor
/// claim is completed with an explicit warning, because a claim left held
/// would pin the checkpoint forever.
const STRANDED_TRIGGER_LIMIT: usize = 16;

/// A trigger whose attempt reached no decision, held for exactly one re-drive.
///
/// The cursor claim stays **in flight** for the whole hold rather than being
/// abandoned and re-admitted: in-flight is the honest state (the daemon has
/// not finished with this event), it pins the checkpoint identically, and
/// re-admitting would advance [`crate::cursor::Cursor::covered_through_secs`]
/// off a retained event — which is not evidence the feed is current and would
/// under-report a real outage.
struct StrandedTrigger {
    event: TriggerEvent,
    /// When the re-drive may run. Past the deploy debounce, so an attempt
    /// whose predecessor stamped it is not refused for that same reason again.
    due_at: tokio::time::Instant,
}

/// What the loop must do with a finished attempt.
enum Settlement {
    /// The cursor was settled here; nothing further is owed.
    Settled,
    /// The attempt neither proved liveness nor made a policy decision, so the
    /// mention it was for has not been acted on. The cursor claim is
    /// deliberately left held and the caller must retain this trigger for one
    /// re-drive past the deploy debounce.
    Strand(TriggerEvent),
}

/// Gap left between re-subscribes when draining parked channels.
///
/// Every channel parked by one burst of rate limiting comes due at roughly
/// the same moment, and firing all of their REQs at once is what tripped the
/// quota in the first place. Spreading the drain is this loop's answer to
/// that; it is why [`rate_limit_park_ms`] itself can stay jitter-free and
/// testable.
const PARK_DRAIN_SPACING: Duration = Duration::from_millis(250);

/// A channel whose live subscription the relay closed as rate limited.
///
/// Lives only as long as one connection — a reconnect re-runs discovery and
/// resubscribes everything, so carrying this across would be stale.
struct ParkedChannel {
    /// When to retry, or `None` once the retry has been sent and this entry
    /// is being kept only for its strike count.
    retry_at: Option<tokio::time::Instant>,
    /// Rate-limited closes this channel has taken on this connection. Drives
    /// the escalation in [`rate_limit_park_ms`], and deliberately survives a
    /// successful re-subscribe: a relay that rate limits the same channel
    /// repeatedly must back off further each time, not restart at the floor.
    strikes: u32,
}

/// Wait until the next parked channel may be re-subscribed, or forever if
/// none is parked.
///
/// `not_before` is the floor left by the previous drain. Holding the spacing
/// here, rather than by pushing each still-parked channel's own deadline
/// forward, is what makes it uniform: channels parked microseconds apart come
/// due microseconds apart, and a rule that only defers the batch already due
/// lets the stragglers through unspaced.
///
/// The `pending` arm is what makes this safe to leave in the `select!`
/// permanently: with nothing parked the branch simply never fires.
async fn next_park_due(
    parked: &HashMap<Uuid, ParkedChannel>,
    not_before: Option<tokio::time::Instant>,
) {
    match parked.values().filter_map(|p| p.retry_at).min() {
        Some(deadline) => {
            tokio::time::sleep_until(not_before.map_or(deadline, |floor| deadline.max(floor)))
                .await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// The next start request to claim: one a failed claim left behind, before
/// anything new from the bundle tap. Cancel-safe for `tokio::select!` — a
/// held request is taken only by the poll that returns it.
async fn next_start_request(
    unclaimed: &mut Option<TriggerEvent>,
    start_requests: &mut mpsc::Receiver<TriggerEvent>,
) -> Option<TriggerEvent> {
    match unclaimed.take() {
        Some(request) => Some(request),
        None => start_requests.recv().await,
    }
}

/// Claim an owner start request in the durable cursor, as a mention is
/// claimed by [`step`].
///
/// `Ok(None)` is a request already claimed or completed — a replayed
/// envelope, recognised by its event id — which must not start the agent
/// again. `Ok(Some(undeliverable))` is a fresh claim to run.
///
/// # Errors
/// The claim could not be made durable; the request must not be acted on.
fn claim_start_request(
    cursor: &mut CursorStore,
    request: &TriggerEvent,
    now: u64,
) -> Result<Option<bool>, crate::cursor::CursorError> {
    Ok(match cursor.admit(&request.id, request.created_at, now)? {
        Admission::Duplicate => None,
        Admission::Fresh => Some(false),
        Admission::FreshButUndeliverable { .. } => Some(true),
    })
}

/// Run one agent's wake loop until `cancel` fires.
///
/// Owns the agent's [`CursorStore`] for the lifetime of the call: opens it
/// once, outliving every individual connection attempt, and closes it (via
/// `Drop`) only when this function returns.
///
/// `start_requests` carries owner start requests the bundle tap admitted
/// ([`crate::start_request`]). Each is claimed in the same cursor and run as
/// the same attempt a mention gets, so it collapses with concurrent mentions
/// and gets the same one re-drive.
pub async fn run_wake_loop(
    config: WakeLoopConfig,
    mut start_requests: mpsc::Receiver<TriggerEvent>,
    cancel: CancellationToken,
) {
    let agent_pubkey = config.keys.public_key().to_hex();

    let mut cursor = match CursorStore::open_or_start(
        config.cursor_path.clone(),
        now_secs(),
        DEFAULT_COMPLETED_RING,
    ) {
        Ok(cursor) => cursor,
        Err(error) => {
            tracing::error!(
                agent = %agent_pubkey,
                %error,
                "buzz-waker: cannot open the durable cursor; this agent's wake loop cannot start"
            );
            return;
        }
    };

    let attempt_state = Arc::new(WakeAttemptState::new());
    let mut attempts: JoinSet<AttemptReport> = JoinSet::new();
    // Outside the reconnect loop on purpose: a debounce window outlives a
    // reconnect, so a trigger held across one must survive it. Its cursor
    // claim survives regardless — that is on disk — but re-driving it depends
    // on this buffer.
    let mut stranded: Vec<StrandedTrigger> = Vec::new();
    // A start request the cursor could not claim, retried first after the
    // reconnect that failure forces. Outside the loop for the same reason as
    // `stranded`: nothing else would re-deliver it (see the claim's `Err` arm).
    let mut unclaimed_start: Option<TriggerEvent> = None;
    let mut consecutive_failures = 0u32;
    // The channels the previous connection ended up watching: its discovery
    // plus every membership change it applied. A channel the next discovery
    // finds beyond these is one the live path never subscribed.
    let mut watched: Option<HashSet<Uuid>> = None;
    // Set by the refresh timer: the next reconnect is a refresh, held to
    // REFRESH_SETUP_DEADLINE.
    let mut refreshing = false;

    'reconnect: while !cancel.is_cancelled() {
        if consecutive_failures > 0 {
            let delay_ms = reconnect_delay_ms(consecutive_failures);
            tracing::warn!(
                agent = %agent_pubkey,
                consecutive_failures,
                delay_ms,
                "buzz-waker: mention feed backing off before reconnecting"
            );
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                () = cancel.cancelled() => break 'reconnect,
            }
        }

        let mut transport = RelayFeed::new(
            config.relay_url.clone(),
            config.keys.clone(),
            config.auth_tag.clone(),
        );
        let setup_deadline = std::mem::take(&mut refreshing)
            .then(|| tokio::time::Instant::now() + config.refresh_setup_deadline);
        if let Err(error) = within(setup_deadline, transport.connect()).await {
            tracing::warn!(agent = %agent_pubkey, %error, "buzz-waker: mention feed connect failed");
            consecutive_failures = consecutive_failures.saturating_add(1);
            continue 'reconnect;
        }

        let resume_at = now_secs();
        let since = match cursor.resume(resume_at) {
            Resume::Since(since) => since,
            Resume::GapTooOld {
                since,
                behind_secs,
                max_age_secs,
            } => {
                tracing::error!(
                    agent = %agent_pubkey,
                    behind_secs,
                    max_age_secs,
                    "buzz-waker: coverage gap exceeds the replay-deliverable window — \
                     resuming from the cursor anyway, but a wake for a mention this old \
                     may start an agent too late for it to see the mention"
                );
                since
            }
        };

        // Required order (see feed.rs's module docs): discover_channels, then
        // subscribe_membership, then subscribe_channel_live for every
        // discovered channel — all before the first subscribe_backfill call,
        // or a mention published in the gap is lost for good. Every one of
        // these calls carries the same `since`, never connect time.
        let discovered_channels = match within(setup_deadline, transport.discover_channels()).await
        {
            Ok(channels) => channels,
            Err(error) => {
                tracing::warn!(agent = %agent_pubkey, %error, "buzz-waker: channel discovery failed");
                consecutive_failures = consecutive_failures.saturating_add(1);
                continue 'reconnect;
            }
        };
        // Membership says which channels this agent is in; it does not say
        // which still accept messages. An archived channel cannot receive a
        // mention at all — the relay refuses the publish — so subscribing to
        // one spends rate-limit budget on an event that cannot occur. Fail
        // open: a probe failure means subscribe to everything, because a
        // wasted subscription is cheap and a wrongly skipped live channel
        // loses every wake in it.
        let archived = match within(
            setup_deadline,
            transport.archived_channels(&discovered_channels),
        )
        .await
        {
            Ok(archived) => archived,
            Err(SetupError::Overran) => {
                tracing::warn!(
                    agent = %agent_pubkey,
                    "buzz-waker: the refresh's setup deadline passed during the archive probe"
                );
                consecutive_failures = consecutive_failures.saturating_add(1);
                continue 'reconnect;
            }
            Err(SetupError::Step(error)) => {
                tracing::warn!(
                    agent = %agent_pubkey, %error,
                    "buzz-waker: could not read channel metadata; subscribing to every \
                     discovered channel including any archived ones"
                );
                HashSet::new()
            }
        };
        let discovered_total = discovered_channels.len();
        let discovered_channels: Vec<Uuid> = discovered_channels
            .into_iter()
            .filter(|channel_id| !archived.contains(channel_id))
            .collect();
        if !archived.is_empty() {
            tracing::info!(
                agent = %agent_pubkey,
                skipped_archived = discovered_total - discovered_channels.len(),
                subscribing = discovered_channels.len(),
                "buzz-waker: skipped archived channels; they cannot receive a mention"
            );
        }
        let discovered_set: HashSet<Uuid> = discovered_channels.iter().copied().collect();
        if let Some(previous) = &watched {
            for channel_id in channels_missed(previous, &discovered_set) {
                tracing::warn!(
                    agent = %agent_pubkey, %channel_id,
                    "buzz-waker: discovery found a channel the previous connection was not \
                     watching — its member-added notification was missed or arrived while \
                     disconnected; subscribing now"
                );
            }
        }
        watched = Some(discovered_set);

        if let Err(error) = within(setup_deadline, transport.subscribe_membership(since)).await {
            tracing::warn!(agent = %agent_pubkey, %error, "buzz-waker: membership watch subscribe failed");
            consecutive_failures = consecutive_failures.saturating_add(1);
            continue 'reconnect;
        }
        for channel_id in discovered_channels {
            if let Err(error) = within(
                setup_deadline,
                transport.subscribe_channel_live(channel_id, since),
            )
            .await
            {
                tracing::warn!(
                    agent = %agent_pubkey, %channel_id, %error,
                    "buzz-waker: channel live subscribe failed"
                );
                consecutive_failures = consecutive_failures.saturating_add(1);
                continue 'reconnect;
            }
        }
        if let Err(error) = within(setup_deadline, transport.subscribe_backfill(since, None)).await
        {
            tracing::warn!(agent = %agent_pubkey, %error, "buzz-waker: backfill subscribe failed");
            consecutive_failures = consecutive_failures.saturating_add(1);
            continue 'reconnect;
        }
        consecutive_failures = 0;
        // Until the backlog replay completes, a refresh is still inside its
        // setup deadline (see REFRESH_SETUP_DEADLINE).
        let mut replay_done = false;
        // A backfill page the relay refused as rate limited: when to ask for
        // it again, and its bounds.
        let mut backfill_retry: Option<(tokio::time::Instant, u64, Option<u64>)> = None;
        let refresh_at = tokio::time::Instant::now() + config.channel_refresh;
        let mut replay = WakeReplay::new(since);
        // Per-connection: this socket's subscriptions are the only ones these
        // deadlines describe, and the reconnect above already resubscribed
        // everything from discovery.
        let mut parked: HashMap<Uuid, ParkedChannel> = HashMap::new();
        // Floor for the next drain, so re-subscribes stay spaced even when a
        // whole batch of channels came due at once.
        let mut next_drain_at: Option<tokio::time::Instant> = None;

        loop {
            tokio::select! {
                biased;

                () = cancel.cancelled() => {
                    break 'reconnect;
                }

                joined = join_next_or_pending(&mut attempts) => {
                    match joined {
                        Ok(report) => {
                            if let Settlement::Strand(event) =
                                apply_report(&mut cursor, &agent_pubkey, report)
                            {
                                retain_stranded(
                                    &mut stranded,
                                    &mut cursor,
                                    &agent_pubkey,
                                    event,
                                );
                            }
                        }
                        Err(join_error) => tracing::error!(
                            agent = %agent_pubkey,
                            %join_error,
                            "buzz-waker: a wake attempt task panicked; its cursor claim \
                             stays held until the next restart retries it"
                        ),
                    }
                }

                () = next_stranded_due(&stranded) => {
                    // Exactly one re-drive per firing, for the same reason the
                    // park drain above takes one per firing: spawning a batch
                    // here would stop this loop calling `next_frame` for as
                    // long as the spawn burst takes, and every re-drive that
                    // is due is already 125s late — one more turn of the
                    // select! costs it nothing.
                    let now = tokio::time::Instant::now();
                    let due = stranded
                        .iter()
                        .position(|held| held.due_at <= now);
                    if let Some(index) = due {
                        let held = stranded.remove(index);
                        // Recomputed, never carried from admission: the
                        // trigger has aged by at least the debounce window
                        // while held, so a mention that was inside a woken
                        // harness's replay reach when it arrived can have
                        // fallen outside it by now. This is the same test
                        // `CursorStore::admit` applies, applied to the age the
                        // re-drive actually starts at.
                        let undeliverable = now_secs().saturating_sub(held.event.created_at)
                            > crate::WAKE_DELIVERABLE_AGE_SECS;
                        tracing::info!(
                            agent = %agent_pubkey,
                            event_id = %held.event.id,
                            undeliverable,
                            "buzz-waker: re-driving a wake trigger its first attempt left \
                             unresolved"
                        );
                        spawn_attempt(
                            &mut attempts,
                            held.event,
                            undeliverable,
                            true,
                            agent_pubkey.clone(),
                            Arc::clone(&attempt_state),
                            Arc::clone(&config.presence_state),
                            config.watch_list.clone(),
                            config.bundle_state.current(),
                            config.provider_env.clone(),
                            cancel.clone(),
                        );
                    }
                }

                () = next_park_due(&parked, next_drain_at) => {
                    let now = tokio::time::Instant::now();
                    // Exactly one re-subscribe per firing. Sleeping through a
                    // whole batch here would stop this loop calling
                    // `next_frame` for the length of the drain, and a healthy
                    // connection would start to look idle-then-dead with the
                    // relay's pings unanswered — the same reason attempts are
                    // spawned rather than run inline (see the module docs).
                    // One per firing keeps `next_frame` polled between each,
                    // and the spacing floor keeps a burst of parked channels
                    // from resubscribing together and re-tripping the quota
                    // that parked them.
                    let due = parked
                        .iter()
                        .filter(|(_, p)| p.retry_at.is_some_and(|at| at <= now))
                        // Oldest deadline first, then by id so a drain is
                        // reproducible in the logs.
                        .min_by_key(|(channel_id, p)| (p.retry_at, **channel_id))
                        .map(|(channel_id, _)| *channel_id);

                    if let Some(channel_id) = due {
                        next_drain_at = Some(now + PARK_DRAIN_SPACING);
                        if let Err(error) =
                            transport.subscribe_channel_live(channel_id, since).await
                        {
                            tracing::warn!(
                                agent = %agent_pubkey, %channel_id, %error,
                                "buzz-waker: parked channel re-subscribe failed"
                            );
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            continue 'reconnect;
                        }
                        // Clear the deadline but keep the strike count: if the
                        // relay rate limits this channel again, the next park
                        // must be longer, not another turn at the floor.
                        if let Some(entry) = parked.get_mut(&channel_id) {
                            entry.retry_at = None;
                        }
                        tracing::info!(
                            agent = %agent_pubkey, %channel_id,
                            "buzz-waker: re-subscribed a channel parked by rate limiting"
                        );
                    }
                }

                // Lowest priority of the timers, and a plain reconnect: the
                // reconnect path is what re-runs discovery and backfills from
                // before the checkpoint — see CHANNEL_REFRESH_INTERVAL.
                () = tokio::time::sleep_until(refresh_at) => {
                    tracing::info!(
                        agent = %agent_pubkey,
                        "buzz-waker: refreshing the mention feed to rediscover channels"
                    );
                    refreshing = true;
                    continue 'reconnect;
                }

                () = sleep_until_some(backfill_retry.map(|(at, _, _)| at)) => {
                    if let Some((_, since, until)) = backfill_retry.take() {
                        if let Err(error) = transport.subscribe_backfill(since, until).await {
                            tracing::warn!(
                                agent = %agent_pubkey, %error,
                                "buzz-waker: re-issuing a rate-limited backlog page failed"
                            );
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            continue 'reconnect;
                        }
                        tracing::info!(
                            agent = %agent_pubkey,
                            "buzz-waker: re-issued a backlog replay page after rate limiting"
                        );
                    }
                }

                // A refresh whose backlog replay has not completed by its
                // deadline has not recovered anything in time; fail it.
                () = sleep_until_some(setup_deadline), if !replay_done => {
                    tracing::warn!(
                        agent = %agent_pubkey,
                        "buzz-waker: the refresh's backlog replay did not complete within its \
                         setup deadline; reconnecting"
                    );
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    continue 'reconnect;
                }

                // Only polled while the mention feed is connected, on purpose:
                // `CursorStore::admit` records live coverage through `now`,
                // which is true only while the feed is up. A request that
                // arrives during a reconnect waits in the channel.
                Some(request) = next_start_request(&mut unclaimed_start, &mut start_requests) => {
                    match claim_start_request(&mut cursor, &request, now_secs()) {
                        Ok(None) => {}
                        Ok(Some(undeliverable)) => {
                            tracing::info!(
                                agent = %agent_pubkey,
                                event_id = %request.id,
                                "buzz-waker: starting the agent on an owner start request"
                            );
                            spawn_attempt(
                                &mut attempts,
                                request,
                                undeliverable,
                                false,
                                agent_pubkey.clone(),
                                Arc::clone(&attempt_state),
                                Arc::clone(&config.presence_state),
                                config.watch_list.clone(),
                                config.bundle_state.current(),
                                config.provider_env.clone(),
                                cancel.clone(),
                            );
                        }
                        Err(error) => {
                            // Not claimed, so not acted on — and not dropped
                            // either: the bundle tap has already handed it over
                            // and will not send it again unless its own
                            // connection happens to cycle. Hold it, and treat
                            // this like the mention path's cursor failure.
                            tracing::error!(
                                agent = %agent_pubkey,
                                event_id = %request.id,
                                %error,
                                "buzz-waker: cursor could not be made durable; holding an \
                                 owner start request and reconnecting"
                            );
                            unclaimed_start = Some(request);
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            continue 'reconnect;
                        }
                    }
                }

                frame = transport.next_frame(FEED_IDLE_TIMEOUT_SECS) => {
                    match frame {
                        Ok(Some(frame)) => {
                            let now = now_secs();
                            match step(&mut cursor, &mut replay, &frame, now) {
                                Ok(FeedStep::Admitted { event, undeliverable }) => {
                                    spawn_attempt(
                                        &mut attempts,
                                        *event,
                                        undeliverable,
                                        false,
                                        agent_pubkey.clone(),
                                        Arc::clone(&attempt_state),
                                        Arc::clone(&config.presence_state),
                                        config.watch_list.clone(),
                                        config.bundle_state.current(),
                                        config.provider_env.clone(),
                                        cancel.clone(),
                                    );
                                }
                                Ok(FeedStep::Nothing) => {}
                                Ok(FeedStep::Rejected { event_id, reason }) => {
                                    tracing::error!(
                                        agent = %agent_pubkey,
                                        %event_id,
                                        %reason,
                                        "buzz-waker: relay served an event on the mention feed \
                                         that failed signature verification"
                                    );
                                }
                                Ok(FeedStep::Backfill { since, until }) => {
                                    if let Err(error) = transport.subscribe_backfill(since, Some(until)).await {
                                        tracing::warn!(
                                            agent = %agent_pubkey, %error,
                                            "buzz-waker: backfill subscribe failed"
                                        );
                                        consecutive_failures = consecutive_failures.saturating_add(1);
                                        continue 'reconnect;
                                    }
                                }
                                Ok(FeedStep::ReplayComplete { truncated, reason }) => {
                                    replay_done = true;
                                    backfill_retry = None;
                                    if truncated {
                                        tracing::error!(
                                            agent = %agent_pubkey,
                                            reason = reason.as_deref().unwrap_or(
                                                "page budget spent or stalled on a tied second"
                                            ),
                                            "buzz-waker: backlog replay ended truncated — some \
                                             historical mentions were not recovered and never \
                                             will be; this is an operational incident, not a \
                                             transient condition"
                                        );
                                    }
                                    if let Err(error) = transport.close_backfill().await {
                                        tracing::warn!(
                                            agent = %agent_pubkey, %error,
                                            "buzz-waker: closing the backfill subscription failed"
                                        );
                                    }
                                }
                                Ok(FeedStep::BackfillRateLimited {
                                    since,
                                    until,
                                    reason,
                                    strikes,
                                }) => {
                                    // Correlated with the subscription burst
                                    // that precedes it (see
                                    // MAX_BACKFILL_RATE_LIMIT_RETRIES): park
                                    // the page past the relay's hint, as the
                                    // channel subscriptions are parked.
                                    let delay_ms = rate_limit_park_ms(
                                        parse_rate_limit_retry_secs(&reason),
                                        strikes,
                                    );
                                    backfill_retry = Some((
                                        tokio::time::Instant::now()
                                            + Duration::from_millis(delay_ms),
                                        since,
                                        until,
                                    ));
                                    tracing::warn!(
                                        agent = %agent_pubkey, %reason, delay_ms, strikes,
                                        "buzz-waker: backlog replay page rate limited; \
                                         re-issuing it after the relay's hint"
                                    );
                                }
                                Ok(FeedStep::Closed(reason)) => {
                                    tracing::warn!(
                                        agent = %agent_pubkey, %reason,
                                        "buzz-waker: relay closed the mention feed subscription"
                                    );
                                    consecutive_failures = consecutive_failures.saturating_add(1);
                                    continue 'reconnect;
                                }
                                Ok(FeedStep::ChannelLiveClosed { channel_id, reason }) => {
                                    // Rate limiting is neither independent nor
                                    // transient: it closes every channel at
                                    // once, and the retry is itself what
                                    // re-trips the quota. Worse, the retry's
                                    // send *succeeds*, so the reconnect ladder
                                    // never counts it and never backs off —
                                    // park instead, and let the drain above
                                    // resubscribe once the relay's own hint
                                    // (floored) has elapsed.
                                    if is_rate_limited(&reason) {
                                        let entry = parked
                                            .entry(channel_id)
                                            .or_insert(ParkedChannel { retry_at: None, strikes: 0 });
                                        let delay_ms = rate_limit_park_ms(
                                            parse_rate_limit_retry_secs(&reason),
                                            entry.strikes,
                                        );
                                        entry.strikes = entry.strikes.saturating_add(1);
                                        entry.retry_at = Some(
                                            tokio::time::Instant::now()
                                                + Duration::from_millis(delay_ms),
                                        );
                                        tracing::warn!(
                                            agent = %agent_pubkey, %channel_id, %reason,
                                            delay_ms, strikes = entry.strikes,
                                            "buzz-waker: channel live subscription rate limited; \
                                             parking before re-subscribing"
                                        );
                                        continue;
                                    }

                                    // The relay closed this channel to the
                                    // agent (archived, or no longer a member).
                                    // Re-subscribing would hold a dead
                                    // subscription against the quota, or be
                                    // refused and bounce; a member-added
                                    // notification brings it back if access
                                    // returns — and so does the next
                                    // refresh's discovery, which must then
                                    // see it as one this connection was not
                                    // watching.
                                    if is_channel_access_denied(&reason) {
                                        parked.remove(&channel_id);
                                        if let Some(watched) = watched.as_mut() {
                                            watched.remove(&channel_id);
                                        }
                                        tracing::info!(
                                            agent = %agent_pubkey, %channel_id, %reason,
                                            "buzz-waker: stopped watching a channel the relay \
                                             closed to this agent"
                                        );
                                        continue;
                                    }

                                    // Any other close is an independent
                                    // failure. Not a full reconnect — see the
                                    // module docs — but left unresubscribed
                                    // until an unrelated reconnect or
                                    // membership event fired would silently
                                    // degrade coverage for this channel
                                    // indefinitely. Retry the one subscription
                                    // immediately; if that itself fails, fall
                                    // back to the ordinary reconnect ladder.
                                    tracing::warn!(
                                        agent = %agent_pubkey, %channel_id, %reason,
                                        "buzz-waker: channel live subscription closed; \
                                         re-subscribing"
                                    );
                                    if let Err(error) =
                                        transport.subscribe_channel_live(channel_id, since).await
                                    {
                                        tracing::warn!(
                                            agent = %agent_pubkey, %channel_id, %error,
                                            "buzz-waker: channel live re-subscribe failed"
                                        );
                                        consecutive_failures = consecutive_failures.saturating_add(1);
                                        continue 'reconnect;
                                    }
                                }
                                Ok(FeedStep::ChannelMembershipChanged { channel_id, added }) => {
                                    // A membership signal is fresher intent
                                    // than any pending park: without this, a
                                    // stale deadline could resubscribe a
                                    // channel the agent just left, or race a
                                    // subscribe it just gained.
                                    parked.remove(&channel_id);
                                    if added {
                                        if let Err(error) =
                                            transport.subscribe_channel_live(channel_id, since).await
                                        {
                                            tracing::warn!(
                                                agent = %agent_pubkey, %channel_id, %error,
                                                "buzz-waker: channel live subscribe failed after \
                                                 a membership-added notification"
                                            );
                                            consecutive_failures = consecutive_failures.saturating_add(1);
                                            continue 'reconnect;
                                        }
                                        tracing::info!(
                                            agent = %agent_pubkey, %channel_id,
                                            "buzz-waker: watching a channel after a \
                                             membership-added notification"
                                        );
                                        if let Some(watched) = watched.as_mut() {
                                            watched.insert(channel_id);
                                        }
                                    } else if let Err(error) =
                                        transport.unsubscribe_channel_live(channel_id).await
                                    {
                                        tracing::warn!(
                                            agent = %agent_pubkey, %channel_id, %error,
                                            "buzz-waker: channel live unsubscribe failed after \
                                             a membership-removed notification"
                                        );
                                        consecutive_failures = consecutive_failures.saturating_add(1);
                                        continue 'reconnect;
                                    } else {
                                        tracing::info!(
                                            agent = %agent_pubkey, %channel_id,
                                            "buzz-waker: stopped watching a channel after a \
                                             membership-removed notification"
                                        );
                                        if let Some(watched) = watched.as_mut() {
                                            watched.remove(&channel_id);
                                        }
                                    }
                                }
                                Err(error) => {
                                    tracing::error!(
                                        agent = %agent_pubkey, %error,
                                        "buzz-waker: cursor could not be made durable; \
                                         reconnecting rather than processing events this \
                                         daemon cannot record"
                                    );
                                    consecutive_failures = consecutive_failures.saturating_add(1);
                                    continue 'reconnect;
                                }
                            }
                        }
                        Ok(None) => {
                            // Idle. Not failure — a quiet channel is the
                            // normal case. Checkpoint coverage so a later
                            // restart can tell "nothing happened" from "we
                            // were not watching".
                            if let Err(error) = cursor.mark_covered(now_secs()) {
                                tracing::warn!(
                                    agent = %agent_pubkey, %error,
                                    "buzz-waker: could not persist the coverage watermark"
                                );
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                agent = %agent_pubkey, %error,
                                "buzz-waker: mention feed connection lost"
                            );
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            continue 'reconnect;
                        }
                    }
                }
            }
        }
    }

    // Shutting down: give in-flight attempts a bounded window to reach a
    // terminal outcome (every `WakeEffects` wait observes `cancel` and exits
    // promptly) so the cursor reflects them before this task — and the
    // cursor's file lock — goes away. Anything still running past the bound
    // is abandoned in memory only; the durable claim stays held and the next
    // restart retries it, per the cursor's own crash-safety contract.
    let shutdown_drain = Duration::from_secs(10);
    let drain_deadline = tokio::time::Instant::now() + shutdown_drain;
    while !attempts.is_empty() {
        let remaining = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            tracing::warn!(
                agent = %agent_pubkey,
                pending = attempts.len(),
                "buzz-waker: shutting down with wake attempts still in flight past the drain \
                 window; their cursor claims stay held for the next restart to retry"
            );
            break;
        }
        tokio::select! {
            joined = join_next_or_pending(&mut attempts) => {
                match joined {
                    Ok(report) => {
                        // A strand here is not retained: this loop is exiting,
                        // so nothing would ever re-drive it. Leaving the claim
                        // in flight — which is what `Settlement::Strand`
                        // already did — is exactly right: it stays held on
                        // disk and the next restart replays it, per the
                        // cursor's crash-safety contract.
                        if let Settlement::Strand(event) =
                            apply_report(&mut cursor, &agent_pubkey, report)
                        {
                            tracing::info!(
                                agent = %agent_pubkey,
                                event_id = %event.id,
                                "buzz-waker: shutting down with a wake trigger unresolved; \
                                 its claim stays held for the next restart to retry"
                            );
                        }
                    }
                    Err(join_error) => tracing::error!(agent = %agent_pubkey, %join_error, "buzz-waker: a wake attempt task panicked during shutdown"),
                }
            }
            () = tokio::time::sleep(remaining) => break,
        }
    }
}

/// [`JoinSet::join_next`], but pending forever (rather than resolving to
/// `None`) when the set is empty — so it composes inside `tokio::select!`
/// without a busy loop on an agent with no attempts in flight.
async fn join_next_or_pending(
    attempts: &mut JoinSet<AttemptReport>,
) -> Result<AttemptReport, tokio::task::JoinError> {
    match attempts.join_next().await {
        Some(result) => result,
        None => std::future::pending().await,
    }
}

/// Does this report leave its mention unacted-on, such that dropping it now
/// would lose it?
///
/// True exactly when the attempt neither proved liveness nor made a policy
/// decision:
///
/// - [`WakeOutcome::Debounced`] — a recent attempt stamped the window, so this
///   trigger was never evaluated at all. The stamping attempt's own deploy may
///   have *failed*, in which case nothing woke and nothing ever will.
/// - [`WakeOutcome::DeployFailed`] — a wake was asked for and did not happen.
/// - [`WakeOutcome::WakeUnconfirmed`] — the deploy landed but no heartbeat
///   followed. Its doc says "the next mention can retry"; without this,
///   nothing retries when no next mention comes.
/// - [`WakeOutcome::PresenceUnavailable`] — a relay hiccup, not a decision.
///
/// Everything else is terminal on purpose:
///
/// - `Woken` / `AlreadyLive` — liveness was proven. (The desktop retains these
///   too, to grade an unverifiable per-channel REQ delivery; the daemon
///   already logs that case as an error and does not re-deploy for it.)
/// - `AuthorRejected` / `AuthorUnverified` — fail-closed policy verdicts.
///   Re-driving would re-litigate a veto that will not change.
/// - `Cancelled` — handled separately: abandoned, not completed.
///
/// [`WakeOutcome::InFlight`] is the one arm that is terminal only
/// *conditionally* — see [`in_flight_owner_covers`].
fn leaves_mention_unacted(report: &AttemptReport) -> bool {
    match report.outcome {
        WakeOutcome::Debounced
        | WakeOutcome::DeployFailed
        | WakeOutcome::WakeUnconfirmed
        | WakeOutcome::PresenceUnavailable => true,
        WakeOutcome::InFlight => !in_flight_owner_covers(report),
        WakeOutcome::Woken
        | WakeOutcome::AlreadyLive
        | WakeOutcome::AuthorRejected
        | WakeOutcome::AuthorUnverified
        | WakeOutcome::Cancelled => false,
    }
}

/// Will the attempt that took the in-flight claim act on this follower's
/// mention too?
///
/// Completing a follower's cursor claim on the strength of someone else's
/// in-flight attempt is a bet, and this is where the bet has to be made
/// good. Three independent things have to hold, and every one of them was
/// assumed rather than checked at some point in this file's history:
///
/// 1. **The owner's wake reaches back this far.** A deploy's replay floor is
///    its own trigger's `created_at` ([`crate::effects::RealWakeEffects`]),
///    so an owner spans a follower only when it is the *older* of the two
///    (within [`crate::decide::WAKE_REPLAY_FLOOR_SKEW_SECS`]). Live traffic
///    mostly arrives oldest-first and made that true by accident; a backlog
///    drain does not, because the relay serves stored rows **newest-first**
///    (see [`CursorStore`]'s module doc) and every admitted row is spawned
///    onto its own task, so which one wins the claim is a lock race.
/// 2. **The owner's author verdict, if it refuses on one, is this follower's
///    too.** The claim is taken per agent *before*
///    `confirm_author_not_known_agent` runs, and that gate is per triggering
///    author. So an agent-authored trigger can own the claim, refuse a human
///    follower, and then exit [`WakeOutcome::AuthorRejected`] — terminal, no
///    deploy, not stranded — having vetoed a mention it never judged. Only a
///    follower with the same author inherits that verdict.
/// 3. **A failing owner will be re-driven.** Points 1 and 2 prove reach, not
///    delivery: they say the owner's floor would span this mention, not that
///    its deploy succeeds. That gap is closed by the owner being stranded and
///    re-driven on its own (older) floor, which spans this mention too — but
///    only for a *first* attempt. A re-driven owner settles terminally
///    whatever it reports (one-shot means one shot, see [`apply_report`]), so
///    there is no next attempt to inherit, and the chain does not exist.
///
/// A `None` owner — which `claim` never produces for this outcome — covers
/// nothing, because an unproven collapse must not be the thing that
/// authorizes dropping a mention.
///
/// Every rejection here costs at most one extra re-driven attempt, which a
/// live agent debounces or no-ops. That asymmetry is why all three checks are
/// written to fail towards retention.
fn in_flight_owner_covers(report: &AttemptReport) -> bool {
    report.in_flight_owner.as_ref().is_some_and(|owner| {
        !owner.retried
            && normalize_pubkey(&owner.author) == normalize_pubkey(&report.event.author)
            && crate::decide::is_covered_by_replay_floor(report.event.created_at, owner.created_at)
    })
}

/// Fold a finished attempt's outcome into the durable cursor.
///
/// [`WakeOutcome::Cancelled`] is abandoned, not completed: the attempt fired
/// its generation fence before reaching any real decision, so the mention is
/// exactly as unprocessed as it was on admission and must be retried once
/// this daemon is not shutting down.
///
/// An outcome that [`leaves_mention_unacted`] returns
/// [`Settlement::Strand`] for — but only on a first attempt. A re-driven
/// attempt's settlement is terminal whatever it reports: one-shot means one
/// shot, and re-stranding would arm timer after timer for as long as the
/// condition persists (a provider that is down stays down), which is exactly
/// the unbounded cycle the desktop's one-shot policy exists to prevent.
fn apply_report(cursor: &mut CursorStore, agent_pubkey: &str, report: AttemptReport) -> Settlement {
    if report.outcome == WakeOutcome::Cancelled {
        cursor.abandon(&report.event.id);
        return Settlement::Settled;
    }
    if leaves_mention_unacted(&report) && !report.retried {
        // The claim stays in flight — see `StrandedTrigger` for why that is
        // the right cursor state for a held trigger.
        return Settlement::Strand(report.event);
    }
    if report.retried && leaves_mention_unacted(&report) {
        // Deliberately says only what is knowable. A batch of stranded
        // triggers re-drives oldest first, and a deploy's replay floor is the
        // triggering event's own `created_at` (`effects::RealWakeEffects`), so
        // a later sibling debounced behind that deploy is very likely inside
        // its floor — but per-channel delivery is observable only on the
        // harness side, so this cannot claim either way.
        tracing::warn!(
            agent = %agent_pubkey,
            event_id = %report.event.id,
            outcome = %report.outcome,
            "buzz-waker: wake trigger reached no resolution on its one re-drive and will \
             not be re-driven again; a new mention starts fresh"
        );
    }
    if let Err(error) = cursor.complete(&report.event.id) {
        tracing::error!(
            agent = %agent_pubkey,
            event_id = %report.event.id,
            outcome = %report.outcome,
            %error,
            "buzz-waker: could not durably complete a wake attempt's cursor claim; \
             it stays held and will be retried"
        );
    }
    Settlement::Settled
}

/// Retain a trigger for its one re-drive, evicting the oldest past the bound.
///
/// Dedupes by event id for the same reason the desktop's
/// `pushBoundedPendingTrigger` does: the same event can reach this loop twice
/// (reconnect replay overlapping the live window), and a duplicate would spend
/// a second re-drive on one mention.
///
/// Eviction is by the trigger's own `created_at`, and this is where the
/// daemon's copy of the rule has to diverge from the desktop's. The desktop
/// drops the front of the queue, which is the oldest mention *because its tap
/// has no history replay* — its own doc says so, and every event it sees is
/// live and in order. This loop drains history, and the relay serves stored
/// rows newest-first, so a front-drop here would discard the **newest**
/// mentions and keep the stalest ones — precisely inverting "newest mentions
/// are the most actionable". Insertion order breaks ties so a drop is still
/// reproducible in the logs.
fn retain_stranded(
    stranded: &mut Vec<StrandedTrigger>,
    cursor: &mut CursorStore,
    agent_pubkey: &str,
    event: TriggerEvent,
) {
    if stranded.iter().any(|held| held.event.id == event.id) {
        return;
    }
    stranded.push(StrandedTrigger {
        event,
        due_at: tokio::time::Instant::now() + Duration::from_millis(WAKE_STRANDED_RETRY_DELAY_MS),
    });
    while stranded.len() > STRANDED_TRIGGER_LIMIT {
        let Some(oldest) = stranded
            .iter()
            .enumerate()
            .min_by_key(|(index, held)| (held.event.created_at, *index))
            .map(|(index, _)| index)
        else {
            break;
        };
        let evicted = stranded.remove(oldest);
        // Never silent, and never left claimed: an evicted trigger's claim
        // would otherwise pin the checkpoint for the life of the process.
        tracing::warn!(
            agent = %agent_pubkey,
            event_id = %evicted.event.id,
            limit = STRANDED_TRIGGER_LIMIT,
            "buzz-waker: dropping the oldest unresolved wake trigger to stay inside the \
             retention bound; it will not be re-driven"
        );
        if let Err(error) = cursor.complete(&evicted.event.id) {
            tracing::error!(
                agent = %agent_pubkey,
                event_id = %evicted.event.id,
                %error,
                "buzz-waker: could not durably complete an evicted wake trigger's cursor claim"
            );
        }
    }
}

/// Wait until the earliest stranded trigger is due for its re-drive, or
/// forever if none is held.
///
/// The `pending` arm is what makes this safe to leave in the `select!`
/// permanently: with nothing stranded the branch simply never fires.
async fn next_stranded_due(stranded: &[StrandedTrigger]) {
    match stranded.iter().map(|held| held.due_at).min() {
        Some(due_at) => tokio::time::sleep_until(due_at).await,
        None => std::future::pending().await,
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_attempt(
    attempts: &mut JoinSet<AttemptReport>,
    event: TriggerEvent,
    undeliverable: bool,
    retried: bool,
    agent_pubkey: String,
    attempt_state: Arc<WakeAttemptState>,
    presence_state: Arc<PresenceState>,
    watch_list: WatchList,
    bundle: Option<Arc<crate::bundle::LaunchBundleBody>>,
    provider_env: Option<Arc<HashMap<String, String>>>,
    cancel: CancellationToken,
) {
    attempts.spawn(async move {
        let event_id = event.id.clone();
        let deploy_log_agent = agent_pubkey.clone();
        let deploy_log_event = event_id.clone();
        let effects = RealWakeEffects::new(
            presence_state,
            watch_list,
            &agent_pubkey,
            &event.author,
            event.created_at,
            bundle,
            provider_env,
            cancel,
            move || {
                tracing::info!(
                    agent = %deploy_log_agent,
                    event_id = %deploy_log_event,
                    "buzz-waker: wake deploy accepted; agent should be starting"
                );
            },
        );

        // The claim this attempt is about to take records the trigger it runs
        // for, not just this agent — see `in_flight_owner_covers` for the three
        // things a follower refused by it has to check before letting itself be
        // collapsed into it.
        let claiming = ClaimingTrigger {
            created_at: event.created_at,
            author: event.author.clone(),
            retried,
        };

        let result = run_wake_attempt(&agent_pubkey, &claiming, &attempt_state, &effects).await;

        if undeliverable && result.outcome == WakeOutcome::Woken {
            // The event was already older than a woken harness's replay
            // window at admission (`Admission::FreshButUndeliverable`). The
            // deploy itself succeeded, but the mention that caused it is not
            // guaranteed to be shown — an operational gap, not a healthy
            // wake, however the raw outcome reads.
            tracing::error!(
                agent = %agent_pubkey,
                event_id = %event_id,
                "buzz-waker: wake succeeded but the triggering mention was already too old \
                 for the woken harness's replay floor to reach it"
            );
        }

        if result.outcome == WakeOutcome::Woken && result.floor_adopted != Some(true) {
            // Only `Some(true)` proves this deploy's `BUZZ_ACP_REPLAY_FLOOR`
            // env is in effect anywhere. `Some(false)` is the provider
            // proving a strict no-op against an already-running generation;
            // `None` is a provider that gave no classification at all
            // (reachable for any provider predating this optional wire
            // field) — both are unproven, not just the former. Either way
            // the heartbeat that satisfied `Woken` may be the old
            // generation's, so a recovered mention may fall outside that
            // generation's subscription window — the same operational gap
            // as the undeliverable case above, from a different cause.
            let reason = if result.floor_adopted == Some(false) {
                "the provider proved this deploy was a strict no-op against an \
                 already-running generation"
            } else {
                "the provider gave no fresh-generation classification for this deploy"
            };
            tracing::error!(
                agent = %agent_pubkey,
                event_id = %event_id,
                reason,
                "buzz-waker: wake attempt reported Woken with an unproven replay floor — the \
                 triggering mention is not guaranteed to reach the already-running harness"
            );
        }

        match &result.error {
            Some(error) => tracing::warn!(
                agent = %agent_pubkey,
                event_id = %event_id,
                outcome = %result.outcome,
                %error,
                "buzz-waker: wake attempt ended"
            ),
            None => tracing::info!(
                agent = %agent_pubkey,
                event_id = %event_id,
                outcome = %result.outcome,
                "buzz-waker: wake attempt ended"
            ),
        }

        AttemptReport {
            event,
            outcome: result.outcome,
            retried,
            in_flight_owner: result.in_flight_owner,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;

    fn store(dir: &tempfile::TempDir) -> CursorStore {
        CursorStore::open_or_start(dir.path().join("cursor.json"), NOW, DEFAULT_COMPLETED_RING)
            .expect("open")
    }

    fn trigger(id: &str) -> TriggerEvent {
        trigger_at(id, NOW)
    }

    fn trigger_at(id: &str, created_at: u64) -> TriggerEvent {
        trigger_by(id, created_at, &author_a())
    }

    fn trigger_by(id: &str, created_at: u64, author: &str) -> TriggerEvent {
        TriggerEvent {
            id: id.to_string(),
            author: author.to_string(),
            kind: 9,
            p_tags: vec!["b".repeat(64)],
            created_at,
        }
    }

    /// The author every trigger carries unless a test is specifically about
    /// two of them — the author gate is per triggering author, so a follower
    /// only inherits its owner's verdict when they match.
    fn author_a() -> String {
        "a".repeat(64)
    }

    fn author_b() -> String {
        "b".repeat(64)
    }

    #[test]
    fn a_replayed_start_request_never_starts_the_agent_twice() {
        // The bundle tap re-sends every in-window request on each reconnect.
        // The cursor claim, keyed by envelope id, is the only thing that
        // stops a replay from spending a second deploy — while in flight
        // and after completion alike.
        let dir = tempfile::tempdir().unwrap();
        let mut cursor = store(&dir);
        let request = trigger("start-1");

        assert_eq!(
            claim_start_request(&mut cursor, &request, NOW).unwrap(),
            Some(false)
        );
        assert_eq!(
            claim_start_request(&mut cursor, &request, NOW + 5).unwrap(),
            None
        );
        cursor.complete(&request.id).unwrap();
        assert_eq!(
            claim_start_request(&mut cursor, &request, NOW + 60).unwrap(),
            None
        );

        // A restarted daemon reading the same cursor file refuses it too.
        drop(cursor);
        let mut reopened = store(&dir);
        assert_eq!(
            claim_start_request(&mut reopened, &request, NOW + 90).unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_start_request_whose_claim_failed_is_offered_again_before_new_ones() {
        let (tx, mut rx) = mpsc::channel(4);
        tx.send(trigger("start-new")).await.unwrap();
        let mut unclaimed = Some(trigger("start-held"));

        assert_eq!(
            next_start_request(&mut unclaimed, &mut rx)
                .await
                .unwrap()
                .id,
            "start-held"
        );
        assert!(unclaimed.is_none());
        assert_eq!(
            next_start_request(&mut unclaimed, &mut rx)
                .await
                .unwrap()
                .id,
            "start-new"
        );
    }

    #[test]
    fn an_old_start_request_is_claimed_as_undeliverable() {
        let dir = tempfile::tempdir().unwrap();
        let mut cursor = store(&dir);
        let old = trigger_at("start-old", NOW - crate::WAKE_DELIVERABLE_AGE_SECS - 1);
        assert_eq!(
            claim_start_request(&mut cursor, &old, NOW).unwrap(),
            Some(true)
        );
    }

    fn report(id: &str, outcome: WakeOutcome, retried: bool) -> AttemptReport {
        AttemptReport {
            event: trigger(id),
            outcome,
            retried,
            in_flight_owner: None,
        }
    }

    /// A follower collapsed behind a concurrent attempt, with both triggers'
    /// timestamps spelled out — the ordering is the whole point.
    ///
    /// Owner and follower share an author and the owner is a first attempt, so
    /// this isolates the reach question; tests about the other two conditions
    /// override `in_flight_owner` on the report they get back.
    fn in_flight_report(
        id: &str,
        follower_created_at: u64,
        owner_created_at: u64,
    ) -> AttemptReport {
        AttemptReport {
            event: trigger_at(id, follower_created_at),
            outcome: WakeOutcome::InFlight,
            retried: false,
            in_flight_owner: Some(ClaimingTrigger {
                created_at: owner_created_at,
                author: author_a(),
                retried: false,
            }),
        }
    }

    /// The bug this exists for. A deploy that failed stamps the debounce, so
    /// the mentions that follow it are refused without being looked at — and
    /// completing them there is what silently loses them.
    #[test]
    fn a_debounced_trigger_is_stranded_rather_than_completed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cursor = store(&dir);
        cursor.admit("e1", NOW, NOW).expect("admit");

        let settlement = apply_report(
            &mut cursor,
            "agent",
            report("e1", WakeOutcome::Debounced, false),
        );

        assert!(matches!(settlement, Settlement::Strand(event) if event.id == "e1"));
        // The claim is still held: nothing about this mention is finished.
        assert_eq!(cursor.in_flight_len(), 1);
        assert_eq!(cursor.abandoned_len(), 0);
    }

    /// The failure JC hit: the deploy itself failed, and nothing re-drove it.
    #[test]
    fn a_failed_deploy_is_stranded_for_one_re_drive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cursor = store(&dir);
        cursor.admit("e1", NOW, NOW).expect("admit");

        let settlement = apply_report(
            &mut cursor,
            "agent",
            report("e1", WakeOutcome::DeployFailed, false),
        );

        assert!(matches!(settlement, Settlement::Strand(_)));
        assert_eq!(cursor.in_flight_len(), 1);
    }

    /// One-shot means one shot: a re-driven attempt settles terminally
    /// whatever it reports, or a provider that stays down arms timer after
    /// timer forever.
    #[test]
    fn a_re_driven_trigger_settles_terminally_even_when_unresolved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cursor = store(&dir);
        cursor.admit("e1", NOW, NOW).expect("admit");

        let settlement = apply_report(
            &mut cursor,
            "agent",
            report("e1", WakeOutcome::Debounced, true),
        );

        assert!(matches!(settlement, Settlement::Settled));
        assert_eq!(cursor.in_flight_len(), 0);
        assert_eq!(cursor.abandoned_len(), 0);
    }

    /// A proven wake, and the two fail-closed policy verdicts, must not be
    /// re-driven — re-litigating a veto spends a deploy on an answer that
    /// will not change.
    #[test]
    fn proven_and_vetoed_outcomes_stay_terminal() {
        for outcome in [
            WakeOutcome::Woken,
            WakeOutcome::AlreadyLive,
            WakeOutcome::AuthorRejected,
            WakeOutcome::AuthorUnverified,
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut cursor = store(&dir);
            cursor.admit("e1", NOW, NOW).expect("admit");

            let settlement = apply_report(&mut cursor, "agent", report("e1", outcome, false));

            assert!(
                matches!(settlement, Settlement::Settled),
                "{outcome} must settle terminally"
            );
            assert_eq!(
                cursor.in_flight_len(),
                0,
                "{outcome} must release its claim"
            );
        }
    }

    /// The ordering-neutral half of the collapse, and the reason `InFlight`
    /// is terminal at all: a follower no older than the owner sits inside the
    /// floor the owner's deploy will commit, so re-driving it would spend a
    /// second deploy for a mention the first one already reaches.
    #[test]
    fn a_follower_covered_by_the_owning_attempt_settles_terminally() {
        for (label, follower_at, owner_at) in [
            ("owner is older", NOW, NOW - 600),
            ("same instant", NOW, NOW),
            (
                "owner newer, but inside the skew",
                NOW,
                NOW + crate::decide::WAKE_REPLAY_FLOOR_SKEW_SECS,
            ),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut cursor = store(&dir);
            cursor.admit("e1", NOW, NOW).expect("admit");

            let settlement = apply_report(
                &mut cursor,
                "agent",
                in_flight_report("e1", follower_at, owner_at),
            );

            assert!(
                matches!(settlement, Settlement::Settled),
                "{label}: the owner's replay floor reaches this mention"
            );
            assert_eq!(cursor.in_flight_len(), 0, "{label}: claim must be released");
        }
    }

    /// The bug this exists for. A backlog drain is served newest-first and
    /// every admitted row is spawned concurrently, so the *newest* row can win
    /// the in-flight claim. Its replay floor is its own `created_at`, which
    /// cannot reach back to an older sibling — and completing that sibling's
    /// cursor claim is precisely the "dropping wake triggers" this loop is
    /// meant to have stopped.
    #[test]
    fn an_older_follower_behind_a_newer_owner_is_stranded_not_completed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cursor = store(&dir);
        cursor.admit("e-old", NOW, NOW).expect("admit");

        let follower_at = NOW - 600;
        let owner_at = NOW;
        assert!(
            !crate::decide::is_covered_by_replay_floor(follower_at, owner_at),
            "the fixture must actually sit outside the owner's floor"
        );

        let settlement = apply_report(
            &mut cursor,
            "agent",
            in_flight_report("e-old", follower_at, owner_at),
        );

        assert!(
            matches!(settlement, Settlement::Strand(event) if event.id == "e-old"),
            "an uncovered follower must be retained for its re-drive"
        );
        assert_eq!(
            cursor.in_flight_len(),
            1,
            "nothing about this mention is finished, so its claim stays held"
        );
        assert_eq!(cursor.abandoned_len(), 0);
    }

    /// One second past the skew is still uncovered — the boundary is the
    /// whole content of the predicate, so pin it rather than trusting a
    /// comfortably-large fixture.
    #[test]
    fn the_collapse_boundary_is_exactly_the_replay_floor_skew() {
        let owner_at = NOW;
        let skew = crate::decide::WAKE_REPLAY_FLOOR_SKEW_SECS;

        assert!(in_flight_owner_covers(&in_flight_report(
            "e1",
            owner_at - skew,
            owner_at
        )));
        assert!(!in_flight_owner_covers(&in_flight_report(
            "e1",
            owner_at - skew - 1,
            owner_at
        )));
    }

    /// The two-author ordering. An agent-authored trigger takes the per-agent
    /// claim *before* the per-author gate runs, refuses a concurrent human
    /// mention, then exits `AuthorRejected` without deploying and without
    /// being stranded (see `attempt`'s
    /// `an_agent_authored_owner_does_not_speak_for_a_human_follower` for the
    /// interleaving that produces it). The owner is the older trigger here, so
    /// reach alone would authorize the collapse — and completing the
    /// follower's claim on a veto that was never about it loses the mention.
    #[test]
    fn a_follower_authored_by_someone_else_is_not_collapsed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cursor = store(&dir);
        cursor.admit("e1", NOW, NOW).expect("admit");

        let mut report = in_flight_report("e1", NOW, NOW - 1);
        report.event = trigger_by("e1", NOW, &author_b());

        let settlement = apply_report(&mut cursor, "agent", report);

        assert!(matches!(settlement, Settlement::Strand(event) if event.id == "e1"));
        assert_eq!(cursor.in_flight_len(), 1);
    }

    /// The same ordering with one author: whatever verdict the owner reaches
    /// is this follower's verdict too, so the collapse stands. Asserted so the
    /// author check cannot quietly become "never collapse anything".
    #[test]
    fn a_follower_sharing_its_owners_author_is_still_collapsed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cursor = store(&dir);
        cursor.admit("e1", NOW, NOW).expect("admit");

        let settlement = apply_report(&mut cursor, "agent", in_flight_report("e1", NOW, NOW - 1));

        assert!(matches!(settlement, Settlement::Settled));
        assert_eq!(cursor.in_flight_len(), 0);
    }

    /// Authors are compared as keys, not as strings: a relay is free to hand
    /// back either hex case, and a case-driven mismatch would strand every
    /// follower forever rather than only the ones that need it.
    #[test]
    fn the_author_comparison_is_case_insensitive() {
        let mut report = in_flight_report("e1", NOW, NOW - 1);
        report.event = trigger_by("e1", NOW, &author_a().to_uppercase());

        assert!(in_flight_owner_covers(&report));
    }

    /// The collapse's last leg is that a *failing* owner comes back for its
    /// one re-drive on the same older floor. A re-driven owner has already
    /// spent that shot — it settles terminally whatever it reports — so there
    /// is no next attempt for this follower to be carried by.
    #[test]
    fn a_follower_behind_a_re_driven_owner_is_not_collapsed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cursor = store(&dir);
        cursor.admit("e1", NOW, NOW).expect("admit");

        let mut report = in_flight_report("e1", NOW, NOW - 1);
        report
            .in_flight_owner
            .as_mut()
            .expect("owner recorded")
            .retried = true;

        let settlement = apply_report(&mut cursor, "agent", report);

        assert!(matches!(settlement, Settlement::Strand(event) if event.id == "e1"));
        assert_eq!(cursor.in_flight_len(), 1);
    }

    /// An unproven collapse must never be the thing that authorizes dropping
    /// a mention: with no owner recorded, fail towards retention.
    #[test]
    fn an_in_flight_report_with_no_named_owner_is_stranded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cursor = store(&dir);
        cursor.admit("e1", NOW, NOW).expect("admit");

        let settlement = apply_report(
            &mut cursor,
            "agent",
            report("e1", WakeOutcome::InFlight, false),
        );

        assert!(matches!(settlement, Settlement::Strand(event) if event.id == "e1"));
        assert_eq!(cursor.in_flight_len(), 1);
    }

    /// One-shot still means one shot. An uncovered follower gets its single
    /// re-drive and no more, whatever that re-drive reports.
    #[test]
    fn a_re_driven_uncovered_follower_settles_terminally() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cursor = store(&dir);
        cursor.admit("e1", NOW, NOW).expect("admit");

        let mut report = in_flight_report("e1", NOW - 600, NOW);
        report.retried = true;

        let settlement = apply_report(&mut cursor, "agent", report);

        assert!(matches!(settlement, Settlement::Settled));
        assert_eq!(cursor.in_flight_len(), 0);
    }

    /// Unchanged behaviour, asserted so the strand path cannot swallow it: a
    /// fenced attempt reached no decision at all and must stay replayable.
    #[test]
    fn a_cancelled_attempt_is_still_abandoned_not_stranded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cursor = store(&dir);
        cursor.admit("e1", NOW, NOW).expect("admit");

        let settlement = apply_report(
            &mut cursor,
            "agent",
            report("e1", WakeOutcome::Cancelled, false),
        );

        assert!(matches!(settlement, Settlement::Settled));
        assert_eq!(cursor.abandoned_len(), 1);
    }

    #[test]
    fn retention_dedupes_by_event_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cursor = store(&dir);
        let mut stranded = Vec::new();

        retain_stranded(&mut stranded, &mut cursor, "agent", trigger("e1"));
        retain_stranded(&mut stranded, &mut cursor, "agent", trigger("e1"));

        assert_eq!(stranded.len(), 1);
    }

    /// Past the bound the oldest is dropped — and its cursor claim is
    /// completed, not left held. A claim held for an event nothing will ever
    /// re-drive pins the checkpoint for the life of the process.
    #[test]
    fn eviction_past_the_bound_releases_the_evicted_claim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cursor = store(&dir);
        let mut stranded = Vec::new();

        for index in 0..=STRANDED_TRIGGER_LIMIT {
            let id = format!("e{index}");
            cursor.admit(&id, NOW, NOW).expect("admit");
            retain_stranded(&mut stranded, &mut cursor, "agent", trigger(&id));
        }

        assert_eq!(stranded.len(), STRANDED_TRIGGER_LIMIT);
        assert_eq!(stranded[0].event.id, "e1", "the oldest is the one dropped");
        assert!(
            !stranded.iter().any(|held| held.event.id == "e0"),
            "e0 was evicted"
        );
        // Every retained trigger still holds its claim; only the evicted one
        // released.
        assert_eq!(cursor.in_flight_len(), STRANDED_TRIGGER_LIMIT);
    }

    /// A backlog drain arrives newest-first, so arrival order is the reverse
    /// of mention age. Evicting the front of the queue there would keep the
    /// stalest mentions and throw away the ones most worth waking for.
    #[test]
    fn eviction_drops_the_oldest_mention_not_the_first_to_arrive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cursor = store(&dir);
        let mut stranded = Vec::new();

        // Newest first, exactly as the relay serves stored rows.
        for index in 0..=STRANDED_TRIGGER_LIMIT {
            let id = format!("e{index}");
            cursor.admit(&id, NOW, NOW).expect("admit");
            let created_at = NOW - index as u64;
            retain_stranded(
                &mut stranded,
                &mut cursor,
                "agent",
                trigger_at(&id, created_at),
            );
        }

        assert_eq!(stranded.len(), STRANDED_TRIGGER_LIMIT);
        let evicted = format!("e{STRANDED_TRIGGER_LIMIT}");
        assert!(
            !stranded.iter().any(|held| held.event.id == evicted),
            "the oldest mention ({evicted}) is the one dropped, not the first to arrive"
        );
        assert!(
            stranded.iter().any(|held| held.event.id == "e0"),
            "the newest mention must survive the bound"
        );
        assert_eq!(cursor.in_flight_len(), STRANDED_TRIGGER_LIMIT);
    }

    /// The retry must land past the window that refused it, or the re-drive is
    /// refused for the very same reason and the one shot is wasted.
    #[test]
    fn the_re_drive_is_scheduled_past_the_deploy_debounce() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cursor = store(&dir);
        let mut stranded = Vec::new();
        let before = tokio::time::Instant::now();

        retain_stranded(&mut stranded, &mut cursor, "agent", trigger("e1"));

        let held_for = stranded[0].due_at.saturating_duration_since(before);
        assert!(
            held_for.as_millis() as u64 >= crate::attempt::WAKE_ATTEMPT_DEBOUNCE_MS,
            "a re-drive at {held_for:?} lands inside the debounce window"
        );
    }

    #[test]
    fn channels_missed_are_exactly_the_newly_discovered_ones() {
        let (a, b, c) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let previous: HashSet<Uuid> = [a, b].into();
        let discovered: HashSet<Uuid> = [c, a].into();
        // b left (handled by its removal notification, not this warning);
        // c is the one the live watch never had.
        assert_eq!(channels_missed(&previous, &discovered), vec![c]);
        assert!(channels_missed(&discovered, &discovered).is_empty());
    }

    /// A relay double for the whole loop: NIP-42, then for every REQ —
    /// discovery (kind:39002) answers with the channels `members_on(n)` names
    /// for the n-th connection; every other REQ gets a bare EOSE. Records each
    /// connection's channel live subscriptions.
    async fn refreshing_relay(
        members_on: fn(usize) -> Vec<Uuid>,
    ) -> (String, Arc<std::sync::Mutex<Vec<Vec<Uuid>>>>) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let live = Arc::new(std::sync::Mutex::new(Vec::<Vec<Uuid>>::new()));
        let recorded = Arc::clone(&live);
        tokio::spawn(async move {
            let signer = Keys::generate();
            while let Ok((stream, _)) = listener.accept().await {
                let connection = {
                    let mut live = recorded.lock().unwrap();
                    live.push(Vec::new());
                    live.len() - 1
                };
                let recorded = Arc::clone(&recorded);
                let signer = signer.clone();
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    let challenge = serde_json::json!(["AUTH", "c"]).to_string();
                    let _ = ws.send(WsMessage::Text(challenge.into())).await;
                    while let Some(Ok(WsMessage::Text(text))) = ws.next().await {
                        let Ok(frame) = serde_json::from_str::<Vec<serde_json::Value>>(&text)
                        else {
                            continue;
                        };
                        let reply = |v: serde_json::Value| WsMessage::Text(v.to_string().into());
                        match frame[0].as_str() {
                            Some("AUTH") => {
                                let ok = serde_json::json!(["OK", frame[1]["id"], true, ""]);
                                let _ = ws.send(reply(ok)).await;
                            }
                            Some("REQ") => {
                                let sub = frame[1].as_str().unwrap_or_default().to_string();
                                let discovery = frame[2]["kinds"].as_array().is_some_and(|k| {
                                    k.contains(&serde_json::json!(
                                        buzz_core::kind::KIND_NIP29_GROUP_MEMBERS
                                    ))
                                });
                                if discovery {
                                    for channel in members_on(connection) {
                                        let event = nostr::EventBuilder::new(
                                            nostr::Kind::Custom(
                                                buzz_core::kind::KIND_NIP29_GROUP_MEMBERS as u16,
                                            ),
                                            "",
                                        )
                                        .tags([
                                            nostr::Tag::parse(["d", &channel.to_string()]).unwrap()
                                        ])
                                        .sign_with_keys(&signer)
                                        .unwrap();
                                        let _ = ws
                                            .send(reply(serde_json::json!(["EVENT", sub, event])))
                                            .await;
                                    }
                                }
                                for channel in members_on(connection) {
                                    if sub == crate::feed::wake_live_subscription_id(channel) {
                                        recorded.lock().unwrap()[connection].push(channel);
                                    }
                                }
                                let _ = ws.send(reply(serde_json::json!(["EOSE", sub]))).await;
                            }
                            _ => {}
                        }
                    }
                });
            }
        });
        (format!("ws://{addr}"), live)
    }

    /// The agent joins channel B while the feed is up and its member-added
    /// notification never arrives — the case that left a new issue room
    /// unwatched for days. The periodic refresh must reconnect on its own and
    /// subscribe B.
    #[tokio::test]
    async fn the_refresh_subscribes_a_channel_whose_notification_was_missed() {
        let (a, b) = (Uuid::from_u128(0xA), Uuid::from_u128(0xB));
        fn members_on(connection: usize) -> Vec<Uuid> {
            let (a, b) = (Uuid::from_u128(0xA), Uuid::from_u128(0xB));
            if connection == 0 {
                vec![a]
            } else {
                vec![a, b]
            }
        }
        let (relay, live) = refreshing_relay(members_on).await;

        let dir = tempfile::tempdir().unwrap();
        let config = WakeLoopConfig {
            relay_url: relay,
            keys: Keys::generate(),
            auth_tag: None,
            cursor_path: dir.path().join("cursor.json"),
            presence_state: Arc::new(crate::presence_feed::PresenceState::new()),
            watch_list: crate::watch_list::WatchList::new(),
            bundle_state: Arc::new(crate::bundle_feed::BundleState::new()),
            provider_env: None,
            channel_refresh: Duration::from_millis(300),
            refresh_setup_deadline: REFRESH_SETUP_DEADLINE,
        };
        let (_start_tx, start_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_wake_loop(config, start_rx, cancel.clone()));

        let watched_b = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if live
                    .lock()
                    .unwrap()
                    .iter()
                    .skip(1)
                    .any(|subs| subs.contains(&b))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        cancel.cancel();
        let _ = task.await;
        assert!(
            watched_b.is_ok(),
            "the feed never reconnected to subscribe the missed channel"
        );
        assert_eq!(
            live.lock().unwrap()[0],
            vec![a],
            "the first connection watched only A"
        );
    }

    /// Which reply a [`stalling_relay`] withholds on every connection after
    /// the first — that is, on every refresh.
    #[derive(Clone, Copy)]
    enum Stall {
        Discovery,
        BackfillReplay,
    }

    /// A relay double that answers every REQ with a bare EOSE — except, from
    /// the second connection on, the one `stall` names, which it never
    /// answers.
    async fn stalling_relay(stall: Stall) -> String {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut connection = 0usize;
            while let Ok((stream, _)) = listener.accept().await {
                let refresh = connection > 0;
                connection += 1;
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    let text = |v: serde_json::Value| WsMessage::Text(v.to_string().into());
                    let _ = ws.send(text(serde_json::json!(["AUTH", "c"]))).await;
                    while let Some(Ok(WsMessage::Text(frame))) = ws.next().await {
                        let Ok(frame) = serde_json::from_str::<Vec<serde_json::Value>>(&frame)
                        else {
                            continue;
                        };
                        match frame[0].as_str() {
                            Some("AUTH") => {
                                let ok = serde_json::json!(["OK", frame[1]["id"], true, ""]);
                                let _ = ws.send(text(ok)).await;
                            }
                            Some("REQ") => {
                                let sub = frame[1].as_str().unwrap_or_default().to_string();
                                let discovery = frame[2]["kinds"].as_array().is_some_and(|k| {
                                    k.contains(&serde_json::json!(
                                        buzz_core::kind::KIND_NIP29_GROUP_MEMBERS
                                    ))
                                });
                                let withheld = refresh
                                    && match stall {
                                        Stall::Discovery => discovery,
                                        Stall::BackfillReplay => {
                                            sub == crate::feed::WAKE_BACKFILL_SUBSCRIPTION_ID
                                        }
                                    };
                                if !withheld {
                                    let _ = ws.send(text(serde_json::json!(["EOSE", sub]))).await;
                                }
                            }
                            _ => {}
                        }
                    }
                });
            }
        });
        format!("ws://{addr}")
    }

    /// Runs the real loop against `relay` with a short refresh and setup
    /// deadline, capturing its logs, until `needle` is logged or 5s pass.
    async fn loop_logs_within_the_deadline(relay: String, needle: &str) -> bool {
        run_loop_until_logged(relay, needle, Duration::from_millis(300))
            .await
            .0
    }

    /// [`loop_logs_within_the_deadline`] with the refresh interval given,
    /// also returning everything the loop logged.
    async fn run_loop_until_logged(
        relay: String,
        needle: &str,
        channel_refresh: Duration,
    ) -> (bool, String) {
        #[derive(Clone, Default)]
        struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Capture {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let dir = tempfile::tempdir().unwrap();
        let config = WakeLoopConfig {
            relay_url: relay,
            keys: Keys::generate(),
            auth_tag: None,
            cursor_path: dir.path().join("cursor.json"),
            presence_state: Arc::new(crate::presence_feed::PresenceState::new()),
            watch_list: crate::watch_list::WatchList::new(),
            bundle_state: Arc::new(crate::bundle_feed::BundleState::new()),
            provider_env: None,
            channel_refresh,
            refresh_setup_deadline: Duration::from_millis(200),
        };
        let (_start_tx, start_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_wake_loop(config, start_rx, cancel.clone()));
        let logged = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if String::from_utf8_lossy(&capture.0.lock().unwrap()).contains(needle) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok();
        cancel.cancel();
        let _ = task.await;
        let text = String::from_utf8_lossy(&capture.0.lock().unwrap()).into_owned();
        (logged, text)
    }

    /// PR #199 review, round 2. The refresh timer only starts recovery; the
    /// setup after it must be bounded too. A refresh whose discovery never
    /// answers must give up at its setup deadline — without it the loop sat
    /// out the 20s discovery timeout, well past the recovery budget.
    #[tokio::test]
    async fn a_refresh_whose_setup_stalls_gives_up_at_its_deadline() {
        let relay = stalling_relay(Stall::Discovery).await;
        assert!(
            loop_logs_within_the_deadline(relay, "the refresh's setup deadline passed").await,
            "a stalled refresh setup was not abandoned at its deadline"
        );
    }

    /// The missed mention is only admitted during the refresh's backlog
    /// replay, so the deadline runs until that replay completes — not just
    /// until the backfill REQ is sent.
    #[tokio::test]
    async fn a_refresh_whose_backlog_replay_stalls_gives_up_at_its_deadline() {
        let relay = stalling_relay(Stall::BackfillReplay).await;
        assert!(
            loop_logs_within_the_deadline(
                relay,
                "backlog replay did not complete within its setup deadline"
            )
            .await,
            "a stalled refresh replay was not abandoned at its deadline"
        );
    }

    /// A relay double for the whole loop, on every connection: NIP-42,
    /// discovery answering with channel A, a bare EOSE for every other REQ,
    /// and — the first time A is subscribed live on any connection —
    /// `CLOSED restricted: channel access revoked` on it, as the relay sends
    /// to every subscriber when a channel is archived. Returns how many times
    /// A was subscribed live.
    async fn revoking_relay(channel: Uuid) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let live_reqs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&live_reqs);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let counted = Arc::clone(&counted);
                tokio::spawn(serve_revoking(stream, channel, counted));
            }
        });
        (format!("ws://{addr}"), live_reqs)
    }

    /// One connection of [`revoking_relay`].
    async fn serve_revoking(
        stream: tokio::net::TcpStream,
        channel: Uuid,
        counted: Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMessage;
        {
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let signer = Keys::generate();
            let text = |v: serde_json::Value| WsMessage::Text(v.to_string().into());
            let _ = ws.send(text(serde_json::json!(["AUTH", "c"]))).await;
            let live_sub = crate::feed::wake_live_subscription_id(channel);
            while let Some(Ok(WsMessage::Text(frame))) = ws.next().await {
                let Ok(frame) = serde_json::from_str::<Vec<serde_json::Value>>(&frame) else {
                    continue;
                };
                match frame[0].as_str() {
                    Some("AUTH") => {
                        let ok = serde_json::json!(["OK", frame[1]["id"], true, ""]);
                        let _ = ws.send(text(ok)).await;
                    }
                    Some("REQ") => {
                        let sub = frame[1].as_str().unwrap_or_default().to_string();
                        let discovery = frame[2]["kinds"].as_array().is_some_and(|k| {
                            k.contains(&serde_json::json!(
                                buzz_core::kind::KIND_NIP29_GROUP_MEMBERS
                            ))
                        });
                        if discovery {
                            let event = nostr::EventBuilder::new(
                                nostr::Kind::Custom(
                                    buzz_core::kind::KIND_NIP29_GROUP_MEMBERS as u16,
                                ),
                                "",
                            )
                            .tags([nostr::Tag::parse(["d", &channel.to_string()]).unwrap()])
                            .sign_with_keys(&signer)
                            .unwrap();
                            let _ = ws
                                .send(text(serde_json::json!(["EVENT", sub, event])))
                                .await;
                        }
                        if sub == live_sub {
                            let n = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            if n == 0 {
                                let revoked = serde_json::json!([
                                    "CLOSED",
                                    sub,
                                    "restricted: channel access revoked"
                                ]);
                                let _ = ws.send(text(revoked)).await;
                                continue;
                            }
                        }
                        let _ = ws.send(text(serde_json::json!(["EOSE", sub]))).await;
                    }
                    _ => {}
                }
            }
        }
    }

    /// The relay archives a channel and closes its live subscription as
    /// `channel access revoked`. The loop used to re-subscribe at once — and
    /// the relay accepts that REQ, leaving a subscription nothing can ever
    /// post to, held against the quota until the next reconnect. It must drop
    /// the channel and keep the connection.
    #[tokio::test]
    async fn a_channel_the_relay_closes_to_the_agent_is_not_resubscribed() {
        let channel = Uuid::from_u128(0xA);
        let (relay, live_reqs) = revoking_relay(channel).await;

        let dir = tempfile::tempdir().unwrap();
        let config = WakeLoopConfig {
            relay_url: relay,
            keys: Keys::generate(),
            auth_tag: None,
            cursor_path: dir.path().join("cursor.json"),
            presence_state: Arc::new(crate::presence_feed::PresenceState::new()),
            watch_list: crate::watch_list::WatchList::new(),
            bundle_state: Arc::new(crate::bundle_feed::BundleState::new()),
            provider_env: None,
            channel_refresh: CHANNEL_REFRESH_INTERVAL,
            refresh_setup_deadline: REFRESH_SETUP_DEADLINE,
        };
        let (_start_tx, start_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_wake_loop(config, start_rx, cancel.clone()));

        let subscribed = tokio::time::timeout(Duration::from_secs(5), async {
            while live_reqs.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        // Long enough for the CLOSED to arrive and a re-subscribe to follow.
        tokio::time::sleep(Duration::from_secs(1)).await;
        cancel.cancel();
        let _ = task.await;

        assert!(subscribed.is_ok(), "the channel was never subscribed");
        assert_eq!(
            live_reqs.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the loop re-subscribed a channel the relay closed to this agent"
        );
    }

    /// A channel the relay closed to the agent leaves the connection's
    /// watched set with its subscription — otherwise a refresh that finds it
    /// again (unarchived, and its member-added notification missed) would
    /// resubscribe it without saying the live watch had been missing it.
    #[tokio::test]
    async fn a_dropped_channel_is_reported_when_discovery_finds_it_again() {
        let (relay, _) = revoking_relay(Uuid::from_u128(0xA)).await;
        assert!(
            loop_logs_within_the_deadline(relay, "previous connection was not watching").await,
            "a channel the loop dropped was not reported when the refresh found it"
        );
    }

    /// A relay double whose first backfill REQ is refused as rate limited
    /// (with the shortest retry hint the waker honours) and every later one answered, as on a
    /// refresh whose subscription burst tripped the relay's quota.
    async fn rate_limiting_relay() -> String {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let refused = Arc::new(std::sync::atomic::AtomicBool::new(false));
            while let Ok((stream, _)) = listener.accept().await {
                let refused = Arc::clone(&refused);
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    let text = |v: serde_json::Value| WsMessage::Text(v.to_string().into());
                    let _ = ws.send(text(serde_json::json!(["AUTH", "c"]))).await;
                    while let Some(Ok(WsMessage::Text(frame))) = ws.next().await {
                        let Ok(frame) = serde_json::from_str::<Vec<serde_json::Value>>(&frame)
                        else {
                            continue;
                        };
                        match frame[0].as_str() {
                            Some("AUTH") => {
                                let ok = serde_json::json!(["OK", frame[1]["id"], true, ""]);
                                let _ = ws.send(text(ok)).await;
                            }
                            Some("REQ") => {
                                let sub = frame[1].as_str().unwrap_or_default().to_string();
                                let first_backfill = sub
                                    == crate::feed::WAKE_BACKFILL_SUBSCRIPTION_ID
                                    && !refused.swap(true, std::sync::atomic::Ordering::SeqCst);
                                let reply = if first_backfill {
                                    serde_json::json!([
                                        "CLOSED",
                                        sub,
                                        "rate-limited: quota exceeded; retry in 2s"
                                    ])
                                } else {
                                    serde_json::json!(["EOSE", sub])
                                };
                                let _ = ws.send(text(reply)).await;
                            }
                            _ => {}
                        }
                    }
                });
            }
        });
        format!("ws://{addr}")
    }

    /// The real loop, against a relay that rate-limits the first backfill
    /// page: the page is re-issued and the replay completes whole. Before,
    /// the refusal ended the replay truncated and raised an operational
    /// incident on every refresh of every agent in many channels.
    #[tokio::test]
    async fn a_rate_limited_backfill_is_re_issued_not_reported_truncated() {
        // No refresh inside the test: a refresh opens a new connection, whose
        // own backfill supersedes the parked page.
        let (reissued, logs) = run_loop_until_logged(
            rate_limiting_relay().await,
            "re-issued a backlog replay page after rate limiting",
            CHANNEL_REFRESH_INTERVAL,
        )
        .await;
        assert!(
            reissued,
            "the rate-limited backfill page was never re-issued"
        );
        assert!(
            !logs.contains("backlog replay ended truncated"),
            "a rate-limited page was reported as a truncated replay"
        );
    }
}
