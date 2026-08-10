//! The presence tap — build order step 5 ("Operate B") of
//! `PLANS/BUZZ_WAKER_DESIGN.md`.
//!
//! Presence (`buzz_core::kind::KIND_PRESENCE_UPDATE`, kind 20001) is global
//! and ephemeral: the relay never stores it, so a `since`-scoped REQ replays
//! nothing — the only way to know an agent's current status is to hold a live
//! subscription and watch it arrive. This module is that subscription, held
//! once per watched agent and read by [`crate::effects::RealWakeEffects`] for
//! both halves [`crate::attempt::WakeEffects`] needs: `presence()` and
//! `heartbeat()`. One tap answers both, because a live harness republishes
//! `online` on an interval and that republish *is* the heartbeat the attempt
//! state machine watches for — see [`PresenceState`] for exactly what each
//! delivery does to the cached state.
//!
//! # Wire format
//!
//! Confirmed against `buzz-acp::publish_presence`
//! (`crates/buzz-acp/src/lib.rs`) and the relay's own presence synthesis
//! (`crates/buzz-relay/src/api/bridge.rs`, `synthesize_presence`): a
//! kind:20001 event carries no tags, and its `content` is a bare status
//! string — `"online"`, `"away"`, or `"offline"` — not JSON. That HTTP bridge
//! path curates Redis-backed state into the same three values on the way out;
//! this tap reads the raw WebSocket fan-out every publisher (including the
//! bridge's own synthesized replies, on the rare relay that echoes them over
//! WS too) uses, and parses `content` the same way.
//!
//! # Authentication
//!
//! One connection per watched agent, authenticated **as that agent** — the
//! same identity [`crate::relay_feed::RelayFeed`] uses for its mention feed.
//! Presence carries no channel-scoped secret, but the relay's REQ path gates
//! on an authenticated connection regardless of what the filter asks for, so
//! there is no unauthenticated shortcut — and reusing the agent's own keys
//! means no second credential has to be provisioned for this tap.
//!
//! # The cold-start gap this cannot close
//!
//! An agent already online when the tap opens says nothing until its next
//! periodic republish — there is no history to replay it from. Until that
//! first delivery (and again after every reconnect, deliberately — see
//! [`PresenceState::mark_disconnected`]), a lookup reports
//! [`PresenceError::Unresolved`] rather than guessing, so
//! [`WakeEffects::presence`](crate::attempt::WakeEffects::presence) never
//! wrongly reads a live agent as offline. The cost is one republish interval
//! of "unknown" per (re)connect, not a false read.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use buzz_core::kind::KIND_PRESENCE_UPDATE;
use buzz_core::PresenceStatus;
use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};
use nostr::{Keys, Tag};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::attempt::HeartbeatObservation;
use crate::decide::normalize_pubkey;
use crate::feed::reconnect_delay_ms;

/// Subscription id for one agent's presence tap.
///
/// Fixed, like the mention feed's own ids, so a reconnect replaces the old
/// subscription instead of piling up a fresh one, and so relay-side logs stay
/// legible across restarts.
pub const PRESENCE_TAP_SUBSCRIPTION_ID: &str = "buzz-waker-presence";

/// How long to wait for a frame before treating the tap connection as idle.
///
/// Wider than the mention feed's [`crate::feed::FEED_IDLE_TIMEOUT_SECS`]:
/// presence republishes on a multi-minute interval (`buzz-acp`'s
/// `presence_heartbeat`), not on every mention, so a quiet tap is the normal
/// case far more often than a quiet mention feed. There is no cursor to
/// checkpoint here, so unlike the feed's idle timeout this one exists purely
/// to distinguish "quiet" from "the relay stopped answering pings" and force
/// a reconnect attempt in the latter case.
pub const PRESENCE_TAP_IDLE_TIMEOUT_SECS: u64 = 180;

/// Parse a kind:20001 `content` string into the curated status set.
///
/// `None` for anything outside the three known values — an unrecognised
/// status string is exactly as informative as no status at all, and guessing
/// would let a future status value silently read as whichever variant a
/// catch-all picked.
#[must_use]
pub fn parse_presence_content(content: &str) -> Option<PresenceStatus> {
    match content.trim() {
        "online" => Some(PresenceStatus::Online),
        "away" => Some(PresenceStatus::Away),
        "offline" => Some(PresenceStatus::Offline),
        _ => None,
    }
}

/// The REQ filter for one agent's presence tap: global, unscoped to any
/// channel, `authors` pinned to the one watched agent. No `since` — presence
/// carries no history to bound.
#[must_use]
pub fn presence_filter(agent_pubkey: &str) -> Value {
    json!({
        "kinds": [KIND_PRESENCE_UPDATE],
        "authors": [normalize_pubkey(agent_pubkey)],
    })
}

/// The REQ frame opening one agent's presence tap.
#[must_use]
pub fn presence_req(agent_pubkey: &str) -> Value {
    json!([
        "REQ",
        PRESENCE_TAP_SUBSCRIPTION_ID,
        presence_filter(agent_pubkey)
    ])
}

/// Errors reading the tap's cached state.
#[derive(Debug, Clone, Copy, thiserror::Error, PartialEq, Eq)]
pub enum PresenceError {
    /// No presence delivery has resolved the lookup yet on the tap's current
    /// connection — either it has never delivered a first event, or it just
    /// reconnected and is waiting on one. **Not** "offline": collapsing this
    /// into offline would deploy on every reconnect and every cold start,
    /// which is exactly the outage-shaped false positive
    /// [`crate::attempt::WakeEffects::presence`] exists to avoid.
    #[error("presence tap for this agent has not resolved a reading yet")]
    Unresolved,
}

/// Shared, thread-safe cache fed by one agent's presence tap.
///
/// This is the whole answer to "how does [`crate::effects::RealWakeEffects`]
/// get presence and heartbeat data without touching a socket itself": the tap
/// task ([`run_presence_tap`]) owns the connection and writes here; the
/// effects impl only reads.
///
/// # What each delivery does
///
/// - **`online` or `away`** — updates the cached status *and* refreshes the
///   heartbeat entry (this event's id, this machine's clock at delivery).
///   Mirrors the desktop's heartbeat log: any live status republish is
///   evidence of a running harness, which is exactly what
///   [`crate::attempt::LiveEvidenceTracker`] needs two distinct, spaced
///   copies of.
/// - **`offline`** — updates the cached status *and clears* the heartbeat
///   entry. An explicit offline publish is the harness announcing its own
///   exit, and [`crate::attempt::run_wake_attempt`] reads a heartbeat entry
///   disappearing as exactly that signal.
#[derive(Debug, Default)]
pub struct PresenceState {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default, Clone)]
struct Inner {
    status: Option<PresenceStatus>,
    heartbeat: Option<HeartbeatObservation>,
}

impl PresenceState {
    /// A tap with nothing observed yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Recover from a poisoned lock rather than propagating it — a panic in
    /// one reader must not permanently blind every future presence lookup for
    /// this agent.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fold one presence delivery into the cache.
    pub fn observe(&self, event_id: &str, status: PresenceStatus, now_ms: u64) {
        let mut inner = self.lock();
        inner.status = Some(status);
        inner.heartbeat = match status {
            PresenceStatus::Offline => None,
            PresenceStatus::Online | PresenceStatus::Away => Some(HeartbeatObservation {
                event_id: event_id.to_string(),
                observed_at_ms: now_ms,
            }),
        };
    }

    /// Clear the cached **status** at the start of every (re)connection —
    /// called by [`run_presence_tap`] immediately after a fresh connection is
    /// established, before its subscribe is even sent.
    ///
    /// The heartbeat entry is deliberately left untouched: it records real,
    /// local-clock-timestamped deliveries, and a delivery that already
    /// happened is not made less real by the socket it arrived on later
    /// dropping. The status cache is different — it is a point-in-time
    /// snapshot, and serving one from a connection that has since gone away
    /// is exactly the staleness `presence()`'s "fresh, never cached" contract
    /// exists to rule out. Wrong in the dangerous direction, too: a dead
    /// agent's last known "online" would otherwise survive silently across a
    /// reconnect and skip a wake that should happen.
    pub fn mark_disconnected(&self) {
        self.lock().status = None;
    }

    /// The cached status, or [`PresenceError::Unresolved`] if this
    /// connection has not yet delivered a first event.
    ///
    /// # Errors
    /// [`PresenceError::Unresolved`] — see the module note on the cold-start
    /// gap.
    pub fn snapshot(&self) -> Result<Option<PresenceStatus>, PresenceError> {
        match self.lock().status {
            Some(status) => Ok(Some(status)),
            None => Err(PresenceError::Unresolved),
        }
    }

    /// The latest heartbeat delivery, if the log holds one.
    #[must_use]
    pub fn heartbeat(&self) -> Option<HeartbeatObservation> {
        self.lock().heartbeat.clone()
    }
}

/// This machine's clock, milliseconds — never the relay's, never an
/// emitter's. Matches the discipline [`crate::attempt::WakeEffects::now_ms`]
/// documents.
#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Run one agent's presence tap until `cancel` fires.
///
/// Connects, authenticates as `keys`, subscribes under
/// [`PRESENCE_TAP_SUBSCRIPTION_ID`], and folds every delivery into `state`.
/// Reconnects on any transport error using the same ladder the mention feed
/// uses ([`reconnect_delay_ms`]) — there is no cursor to checkpoint here, so
/// reconnect is the entire recovery story: nothing this tap sees is ever
/// claimed or completed, only cached.
///
/// Returns once `cancel` fires. Intended to be spawned as its own task
/// alongside [`crate::wake_loop::run_wake_loop`] for the same agent — see
/// `crates/buzz-waker/src/main.rs`.
pub async fn run_presence_tap(
    relay_url: &str,
    keys: &Keys,
    auth_tag: Option<&Tag>,
    state: &PresenceState,
    cancel: &CancellationToken,
) {
    let agent_pubkey = keys.public_key().to_hex();
    let mut consecutive_failures = 0u32;

    while !cancel.is_cancelled() {
        if consecutive_failures > 0 {
            let delay_ms = reconnect_delay_ms(consecutive_failures);
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                () = cancel.cancelled() => break,
            }
        }

        let connect = NostrWsConnection::connect_authenticated(relay_url, keys, auth_tag);
        let mut connection = tokio::select! {
            result = connect => match result {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::warn!(agent = %agent_pubkey, %error, "presence tap connect failed; backing off");
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    continue;
                }
            },
            () = cancel.cancelled() => break,
        };

        // Before anything else on this connection: a status cached from the
        // connection that just dropped must not answer for this one.
        state.mark_disconnected();

        if let Err(error) = connection.send_raw(&presence_req(&agent_pubkey)).await {
            tracing::warn!(agent = %agent_pubkey, %error, "presence tap subscribe failed; reconnecting");
            consecutive_failures = consecutive_failures.saturating_add(1);
            continue;
        }
        consecutive_failures = 0;

        loop {
            let next = tokio::select! {
                result = connection.next_event(Duration::from_secs(PRESENCE_TAP_IDLE_TIMEOUT_SECS)) => result,
                () = cancel.cancelled() => return,
            };

            match next {
                Ok(RelayMessage::Event {
                    subscription_id,
                    event,
                }) if subscription_id == PRESENCE_TAP_SUBSCRIPTION_ID => {
                    // The same authentication concern as the mention feed:
                    // without a check here a relay could hand this tap a
                    // forged "online" for an agent that is actually dead, and
                    // every wake attempt would treat it as live. See
                    // `crate::relay_feed` for the fuller reasoning.
                    if let Err(error) = buzz_core::verify_event(&event) {
                        tracing::warn!(
                            agent = %agent_pubkey,
                            %error,
                            "presence tap received an event that failed verification; ignoring"
                        );
                        continue;
                    }
                    let Some(status) = parse_presence_content(&event.content) else {
                        continue;
                    };
                    state.observe(&event.id.to_hex(), status, now_ms());
                }
                // A frame for a subscription we did not open, or a message
                // type this tap has no use for (NOTICE, OK, a late AUTH
                // challenge). Ignored, not an error.
                Ok(_) => {}
                Err(WsClientError::Timeout) => {
                    // A quiet tap is the normal case — presence republishes
                    // on a multi-minute interval, not on every mention.
                }
                Err(error) => {
                    tracing::warn!(agent = %agent_pubkey, %error, "presence tap connection lost; reconnecting");
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_three_known_statuses_parse() {
        assert_eq!(
            parse_presence_content("online"),
            Some(PresenceStatus::Online)
        );
        assert_eq!(parse_presence_content("away"), Some(PresenceStatus::Away));
        assert_eq!(
            parse_presence_content("offline"),
            Some(PresenceStatus::Offline)
        );
        assert_eq!(parse_presence_content(""), None);
        assert_eq!(
            parse_presence_content("ONLINE"),
            None,
            "no case-folding, matching the desktop's exact-string convention"
        );
        assert_eq!(
            parse_presence_content("busy"),
            None,
            "an unrecognised status must not guess a variant"
        );
    }

    #[test]
    fn the_filter_carries_no_since_and_pins_one_author() {
        let filter = presence_filter(&"AB".repeat(32));
        assert_eq!(filter["kinds"], json!([KIND_PRESENCE_UPDATE]));
        assert_eq!(filter["authors"], json!(["ab".repeat(32)]));
        assert!(
            filter.get("since").is_none(),
            "presence carries no history to bound"
        );
    }

    #[test]
    fn a_fresh_state_is_unresolved() {
        let state = PresenceState::new();
        assert_eq!(state.snapshot(), Err(PresenceError::Unresolved));
        assert_eq!(state.heartbeat(), None);
    }

    #[test]
    fn an_online_delivery_sets_status_and_a_heartbeat_entry() {
        let state = PresenceState::new();
        state.observe("ev1", PresenceStatus::Online, 1_000);

        assert_eq!(state.snapshot(), Ok(Some(PresenceStatus::Online)));
        let hb = state.heartbeat().expect("a heartbeat entry");
        assert_eq!(hb.event_id, "ev1");
        assert_eq!(hb.observed_at_ms, 1_000);
    }

    #[test]
    fn an_offline_delivery_clears_the_heartbeat_entry() {
        let state = PresenceState::new();
        state.observe("ev1", PresenceStatus::Online, 1_000);
        state.observe("ev2", PresenceStatus::Offline, 2_000);

        assert_eq!(state.snapshot(), Ok(Some(PresenceStatus::Offline)));
        assert_eq!(
            state.heartbeat(),
            None,
            "an announced exit must clear the heartbeat log entry"
        );
    }

    #[test]
    fn a_reconnect_clears_status_but_not_a_real_heartbeat() {
        let state = PresenceState::new();
        state.observe("ev1", PresenceStatus::Online, 1_000);

        state.mark_disconnected();

        assert_eq!(
            state.snapshot(),
            Err(PresenceError::Unresolved),
            "a status from the dropped connection must not answer for the new one"
        );
        assert!(
            state.heartbeat().is_some(),
            "a real, already-delivered heartbeat is not un-happened by a later disconnect"
        );
    }

    #[test]
    fn away_also_refreshes_the_heartbeat_entry() {
        let state = PresenceState::new();
        state.observe("ev1", PresenceStatus::Away, 500);

        assert_eq!(state.snapshot(), Ok(Some(PresenceStatus::Away)));
        assert!(state.heartbeat().is_some());
    }
}
