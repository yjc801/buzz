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
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::attempt::{
    run_wake_attempt, WakeAttemptState, WakeOutcome, WAKE_STRANDED_RETRY_DELAY_MS,
};
use crate::cursor::{CursorStore, Resume, DEFAULT_COMPLETED_RING};
use crate::decide::TriggerEvent;
use crate::effects::RealWakeEffects;
use crate::feed::{
    is_rate_limited, parse_rate_limit_retry_secs, rate_limit_park_ms, reconnect_delay_ms, step,
    FeedStep, FeedTransport, WakeReplay, FEED_IDLE_TIMEOUT_SECS,
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
}

/// Debounce-refused (and otherwise unresolved) triggers retained per agent for
/// their one re-drive, at most.
///
/// Matches the desktop's `WAKE_COLLAPSED_TRIGGER_LIMIT`. One wake loop watches
/// exactly one agent, so this is already the per-agent bound the desktop
/// computes per key. Beyond it the OLDEST is dropped — newest mentions are the
/// most actionable — and dropping is not silent: the evicted trigger's cursor
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

/// Run one agent's wake loop until `cancel` fires.
///
/// Owns the agent's [`CursorStore`] for the lifetime of the call: opens it
/// once, outliving every individual connection attempt, and closes it (via
/// `Drop`) only when this function returns.
pub async fn run_wake_loop(config: WakeLoopConfig, cancel: CancellationToken) {
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
    let mut consecutive_failures = 0u32;

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
        if let Err(error) = transport.connect().await {
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
        let discovered_channels = match transport.discover_channels().await {
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
        let archived = match transport.archived_channels(&discovered_channels).await {
            Ok(archived) => archived,
            Err(error) => {
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

        if let Err(error) = transport.subscribe_membership(since).await {
            tracing::warn!(agent = %agent_pubkey, %error, "buzz-waker: membership watch subscribe failed");
            consecutive_failures = consecutive_failures.saturating_add(1);
            continue 'reconnect;
        }
        for channel_id in discovered_channels {
            if let Err(error) = transport.subscribe_channel_live(channel_id, since).await {
                tracing::warn!(
                    agent = %agent_pubkey, %channel_id, %error,
                    "buzz-waker: channel live subscribe failed"
                );
                consecutive_failures = consecutive_failures.saturating_add(1);
                continue 'reconnect;
            }
        }
        if let Err(error) = transport.subscribe_backfill(since, None).await {
            tracing::warn!(agent = %agent_pubkey, %error, "buzz-waker: backfill subscribe failed");
            consecutive_failures = consecutive_failures.saturating_add(1);
            continue 'reconnect;
        }
        consecutive_failures = 0;
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
                                Ok(FeedStep::ReplayComplete { truncated }) => {
                                    if truncated {
                                        tracing::error!(
                                            agent = %agent_pubkey,
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

/// Does this outcome leave the mention unacted-on, such that dropping it now
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
/// - `InFlight` — an attempt for this same agent owns the decision, and its
///   replay floor is derived from an *earlier* `created_at` than this
///   follower's, so the wake it produces covers this mention.
/// - `AuthorRejected` / `AuthorUnverified` — fail-closed policy verdicts.
///   Re-driving would re-litigate a veto that will not change.
/// - `Cancelled` — handled separately: abandoned, not completed.
fn leaves_mention_unacted(outcome: WakeOutcome) -> bool {
    matches!(
        outcome,
        WakeOutcome::Debounced
            | WakeOutcome::DeployFailed
            | WakeOutcome::WakeUnconfirmed
            | WakeOutcome::PresenceUnavailable
    )
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
    if leaves_mention_unacted(report.outcome) && !report.retried {
        // The claim stays in flight — see `StrandedTrigger` for why that is
        // the right cursor state for a held trigger.
        return Settlement::Strand(report.event);
    }
    if report.retried && leaves_mention_unacted(report.outcome) {
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
        let evicted = stranded.remove(0);
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

        let result = run_wake_attempt(&agent_pubkey, &attempt_state, &effects).await;

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
        TriggerEvent {
            id: id.to_string(),
            author: "a".repeat(64),
            kind: 9,
            p_tags: vec!["b".repeat(64)],
            created_at: NOW,
        }
    }

    fn report(id: &str, outcome: WakeOutcome, retried: bool) -> AttemptReport {
        AttemptReport {
            event: trigger(id),
            outcome,
            retried,
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
            WakeOutcome::InFlight,
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
}
