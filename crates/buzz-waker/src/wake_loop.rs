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

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nostr::{Keys, Tag};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::attempt::{run_wake_attempt, WakeAttemptState, WakeOutcome};
use crate::cursor::{CursorStore, Resume, DEFAULT_COMPLETED_RING};
use crate::decide::TriggerEvent;
use crate::effects::RealWakeEffects;
use crate::feed::{
    reconnect_delay_ms, step, FeedStep, FeedTransport, WakeReplay, FEED_IDLE_TIMEOUT_SECS,
};
use crate::presence_feed::PresenceState;
use crate::relay_feed::RelayFeed;

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
    /// This daemon's full watch list, normalized — see `effects`'s module
    /// doc for why this is the accepted `confirm_author_not_known_agent`
    /// baseline.
    pub watch_list: Arc<[String]>,
    /// The live cache [`crate::bundle_feed::run_bundle_tap`] writes this
    /// agent's admitted bundle into. Read fresh at the moment each attempt is
    /// spawned (never captured once at loop-construction time) so a reissue
    /// admitted mid-run takes effect on the very next wake, with no daemon
    /// restart required.
    pub bundle_state: Arc<crate::bundle_feed::BundleState>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What a finished wake attempt reports back to the loop that spawned it.
struct AttemptReport {
    event_id: String,
    outcome: WakeOutcome,
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

        loop {
            tokio::select! {
                biased;

                () = cancel.cancelled() => {
                    break 'reconnect;
                }

                joined = join_next_or_pending(&mut attempts) => {
                    match joined {
                        Ok(report) => apply_report(&mut cursor, &agent_pubkey, report),
                        Err(join_error) => tracing::error!(
                            agent = %agent_pubkey,
                            %join_error,
                            "buzz-waker: a wake attempt task panicked; its cursor claim \
                             stays held until the next restart retries it"
                        ),
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
                                        agent_pubkey.clone(),
                                        Arc::clone(&attempt_state),
                                        Arc::clone(&config.presence_state),
                                        Arc::clone(&config.watch_list),
                                        config.bundle_state.current(),
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
                                    // Not a full reconnect — see the module
                                    // docs — but left unresubscribed until an
                                    // unrelated reconnect or membership event
                                    // fired would silently degrade coverage
                                    // for this channel indefinitely. Retry
                                    // the one subscription immediately; if
                                    // that itself fails, fall back to the
                                    // ordinary reconnect ladder.
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
                    Ok(report) => apply_report(&mut cursor, &agent_pubkey, report),
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

/// Fold a finished attempt's outcome into the durable cursor.
///
/// [`WakeOutcome::Cancelled`] is abandoned, not completed: the attempt fired
/// its generation fence before reaching any real decision, so the mention is
/// exactly as unprocessed as it was on admission and must be retried once
/// this daemon is not shutting down. Every other outcome — including
/// [`WakeOutcome::DeployFailed`] — is a genuine terminal decision and is
/// completed, per [`crate::attempt::run_wake_attempt`]'s own contract that it
/// always reaches one.
fn apply_report(cursor: &mut CursorStore, agent_pubkey: &str, report: AttemptReport) {
    if report.outcome == WakeOutcome::Cancelled {
        cursor.abandon(&report.event_id);
        return;
    }
    if let Err(error) = cursor.complete(&report.event_id) {
        tracing::error!(
            agent = %agent_pubkey,
            event_id = %report.event_id,
            outcome = %report.outcome,
            %error,
            "buzz-waker: could not durably complete a wake attempt's cursor claim; \
             it stays held and will be retried"
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_attempt(
    attempts: &mut JoinSet<AttemptReport>,
    event: TriggerEvent,
    undeliverable: bool,
    agent_pubkey: String,
    attempt_state: Arc<WakeAttemptState>,
    presence_state: Arc<PresenceState>,
    watch_list: Arc<[String]>,
    bundle: Option<Arc<crate::bundle::LaunchBundleBody>>,
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
            event_id,
            outcome: result.outcome,
        }
    });
}
