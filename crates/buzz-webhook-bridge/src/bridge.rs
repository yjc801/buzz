//! The bridge's connection loop: connect, authenticate, subscribe once per
//! rule, and turn matching deliveries into webhook dispatches.
//!
//! Everything socket-facing lives here; matching and dispatch mechanics stay
//! in [`crate::rules`] and [`crate::dispatch`] where they are unit-tested
//! without a network. The loop itself never exits on a transient error — a
//! failed connection, a relay-closed subscription, or a malformed event all
//! land back in the reconnect ladder, and only cancellation ends the run.
//!
//! The bridge holds a strictly read-only relationship with the relay: the
//! only frames it ever sends are the NIP-42 AUTH handshake (inside
//! `connect_authenticated`) and one REQ per rule. See the crate docs for the
//! loop-safety argument that rests on this.

use std::time::Duration;

use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::config::BridgeConfig;
use crate::dispatch::{
    build_client, deliver, PreparedRequest, SeenRing, TokenBucket, SEEN_RING_CAPACITY,
};
use crate::rules::{rule_matches, EventFields, Rule};

/// Seconds of history each (re)subscribe asks for, so events that arrived
/// while the bridge was down or reconnecting are replayed. The [`SeenRing`]
/// suppresses the duplicates this overlap deliberately produces.
pub const RESUBSCRIBE_OVERLAP_SECS: u64 = 600;

/// How long to wait for a frame before treating the connection as idle. A
/// quiet subscription is the normal case; this exists only to distinguish
/// "quiet" from "the relay stopped answering" — `next_event` answers pings
/// internally, so a healthy-but-quiet relay simply times out here and the
/// loop continues on the same connection.
pub const IDLE_TIMEOUT_SECS: u64 = 60;

/// First reconnect delay; doubles per consecutive failure.
pub const RECONNECT_BASE_DELAY_MS: u64 = 500;

/// Reconnect delay ceiling.
pub const RECONNECT_MAX_DELAY_MS: u64 = 30_000;

/// How long a connection must survive before it counts as healthy without
/// every rule having produced an `EOSE`.
///
/// The `EOSE`-per-rule signal is the primary evidence; this is the fallback
/// for a relay that answers a REQ but never sends `EOSE`, so such a relay
/// degrades to slower reconnects rather than never resetting the ladder. It
/// is deliberately longer than one idle timeout, so an immediate `CLOSED`
/// cannot reach it.
pub const STABLE_CONNECTION_MS: u64 = 120_000;

/// Whether the current connection has proven its subscriptions healthy.
///
/// A successful `send_raw` proves only that REQ bytes reached the socket —
/// not that the relay accepted a single subscription. Resetting the reconnect
/// ladder on that alone lets a relay that immediately `CLOSED`s every
/// subscription (cap reached, policy change, filter it will not serve) hold
/// the bridge in a 500 ms reconnect loop forever: each attempt resets the
/// counter to zero before the `CLOSED` raises it back to one. The failure
/// generation therefore survives the connect and only clears on real
/// acceptance evidence.
///
/// Time is passed in (`now_ms`) rather than read from a clock, matching
/// [`crate::dispatch::TokenBucket`], so the behavior is deterministic under
/// test.
pub struct ConnectionHealth {
    pending: std::collections::BTreeSet<usize>,
    connected_at_ms: u64,
    proven: bool,
}

impl ConnectionHealth {
    /// A fresh connection with `rule_count` subscriptions still unproven.
    #[must_use]
    pub fn new(rule_count: usize, now_ms: u64) -> Self {
        Self {
            pending: (0..rule_count).collect(),
            connected_at_ms: now_ms,
            proven: false,
        }
    }

    /// Record acceptance evidence for one rule's subscription: an `EOSE`, or
    /// an event delivered under it.
    pub fn observe_subscription(&mut self, rule_index: usize) {
        self.pending.remove(&rule_index);
    }

    /// Whether the connection has *just* become proven — true exactly once,
    /// at the transition, so the caller can reset the ladder and log it.
    pub fn became_proven(&mut self, now_ms: u64) -> bool {
        if self.proven {
            return false;
        }
        self.proven = self.pending.is_empty()
            || now_ms.saturating_sub(self.connected_at_ms) >= STABLE_CONNECTION_MS;
        self.proven
    }
}

const SUBSCRIPTION_PREFIX: &str = "bridge-";

/// Capped exponential reconnect backoff — the same ladder `buzz-waker`'s
/// feeds use. `consecutive_failures` is the count *before* this attempt;
/// zero means connect immediately.
#[must_use]
pub fn reconnect_delay_ms(consecutive_failures: u32) -> u64 {
    if consecutive_failures == 0 {
        return 0;
    }
    RECONNECT_BASE_DELAY_MS
        .saturating_mul(1_u64 << consecutive_failures.saturating_sub(1).min(16))
        .min(RECONNECT_MAX_DELAY_MS)
}

/// The subscription id for the rule at `index` in the config's rule list.
#[must_use]
pub fn subscription_id(index: usize) -> String {
    format!("{SUBSCRIPTION_PREFIX}{index}")
}

/// The rule index a subscription id names, if it is one of ours.
#[must_use]
pub fn subscription_rule_index(subscription: &str) -> Option<usize> {
    subscription.strip_prefix(SUBSCRIPTION_PREFIX)?.parse().ok()
}

/// The REQ frame opening one rule's subscription.
///
/// The filter carries the rule's `kinds` and `authors` plus `since` —
/// `d_prefix` is *not* sent: the relay's `#d` filter is exact-match only, so
/// prefix matching happens client-side in [`crate::rules::rule_matches`].
#[must_use]
pub fn rule_req(index: usize, rule: &Rule, since: u64) -> Value {
    json!([
        "REQ",
        subscription_id(index),
        {
            "kinds": rule.filter.kinds,
            "authors": rule.filter.authors,
            "since": since,
        }
    ])
}

/// This machine's clock, unix seconds.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// This machine's clock, milliseconds — feeds the token buckets.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// One webhook the loop decided to send: everything [`deliver`] needs, and
/// nothing that must not be logged.
pub struct Dispatch {
    /// The rule that matched — the identifier its log lines carry.
    pub rule_name: String,
    /// The rule's **unexpanded** url template, safe to log.
    pub url_template: String,
    /// The prepared call itself.
    pub request: PreparedRequest,
}

/// What one relay frame means to the connection loop.
pub struct FrameOutcome {
    /// A webhook to send, when the frame was a matching event.
    pub dispatch: Option<Dispatch>,
    /// The frame proved the connection healthy for the first time — the
    /// reconnect ladder may reset.
    pub proven: bool,
    /// The frame ended this connection; reconnect with backoff.
    pub reconnect: bool,
}

impl FrameOutcome {
    fn nothing() -> Self {
        Self {
            dispatch: None,
            proven: false,
            reconnect: false,
        }
    }
}

/// The reconnect ladder: how many consecutive failed connection generations
/// precede the next attempt.
///
/// Every transition is a named method rather than arithmetic inline in the
/// loop, so the reset rule is something a test can hold. The one that matters
/// is [`ReconnectLadder::connected`], which deliberately does **not** reset:
/// an established socket with its REQ bytes sent is not evidence that the
/// relay accepted a single subscription, and resetting there let a relay that
/// immediately `CLOSED`s every subscription hold the bridge at the bottom
/// rung forever.
#[derive(Default)]
pub struct ReconnectLadder {
    consecutive_failures: u32,
}

impl ReconnectLadder {
    /// A ladder with no failures behind it — connect immediately.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How long to wait before the next connect attempt.
    #[must_use]
    pub fn delay_ms(&self) -> u64 {
        reconnect_delay_ms(self.consecutive_failures)
    }

    /// A connection was established and every rule's REQ was written.
    ///
    /// Intentionally a no-op. `send_raw` returning `Ok` proves the bytes
    /// reached the socket and nothing more; the failure generation survives
    /// until [`ConnectionHealth`] produces real acceptance evidence. If you
    /// are tempted to clear the counter here, read
    /// `a_relay_that_closes_every_subscription_advances_the_backoff_ladder`.
    pub fn connected(&self) {}

    /// The connect or the subscribe write itself failed.
    pub fn connect_failed(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
    }

    /// Fold one frame's outcome in: proof of health clears the generation, a
    /// frame that ended the connection raises it, anything else leaves it
    /// exactly where it was.
    pub fn observe(&mut self, outcome: &FrameOutcome) {
        if outcome.proven {
            self.consecutive_failures = 0;
        } else if outcome.reconnect {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        }
    }
}

/// Decide whether one delivered event frame produces a webhook: verify,
/// match, dedup, budget, prepare.
///
/// Split out from the spawn so the loop's real decision path is exercisable
/// without a socket or an HTTP server — the caller does nothing but spawn
/// what this returns.
fn decide_dispatch(
    config: &BridgeConfig,
    seen: &mut SeenRing,
    buckets: &mut [TokenBucket],
    now_ms: u64,
    subscription: &str,
    event: &nostr::Event,
) -> Option<Dispatch> {
    let rule_index = subscription_rule_index(subscription)?; // not ours
    let rule = config.rules.get(rule_index)?;

    // A structurally valid frame is not a valid event: the ws client parses
    // shape only, so a hostile or buggy relay could otherwise hand this loop
    // a forged author — and the author pin is one of the two loop guards.
    if let Err(error) = buzz_core::verify_event(event) {
        tracing::warn!(
            rule = %rule.name,
            event_id = %event.id.to_hex(),
            %error,
            "event failed verification; skipping"
        );
        return None;
    }

    let fields = EventFields::from_event(event);
    if !rule_matches(rule, &fields) {
        // The relay-side filter cannot express d_prefix, so a non-match here
        // is routine, not suspicious.
        return None;
    }

    // Keyed by (rule, event): one event can match several rules, and each
    // rule's webhook is an independent consumer.
    if !seen.first_sighting(rule_index, &fields.id) {
        tracing::debug!(rule = %rule.name, event_id = %fields.id, "duplicate delivery suppressed");
        return None;
    }

    let bucket = buckets.get_mut(rule_index)?;
    if !bucket.try_take(now_ms) {
        tracing::warn!(
            rule = %rule.name,
            event_id = %fields.id,
            max_per_minute = rule.max_per_minute,
            "over the rule's dispatch budget; dropping this match"
        );
        return None;
    }

    tracing::info!(
        rule = %rule.name,
        event_id = %fields.id,
        kind = fields.kind,
        "rule matched; dispatching webhook"
    );
    Some(Dispatch {
        rule_name: rule.name.clone(),
        url_template: rule.webhook.url.template().to_string(),
        request: PreparedRequest::build(rule, &fields),
    })
}

/// Apply one relay frame to the connection's state.
///
/// This is the whole per-frame decision: the connection loop does nothing
/// with a frame but hand it here and act on the [`FrameOutcome`], which is
/// what lets a test drive the real behavior (cross-rule dispatch, a relay
/// that answers every subscription with `CLOSED`) without a relay.
fn apply_frame(
    config: &BridgeConfig,
    seen: &mut SeenRing,
    buckets: &mut [TokenBucket],
    health: &mut ConnectionHealth,
    now_ms: u64,
    frame: Result<RelayMessage, WsClientError>,
) -> FrameOutcome {
    match frame {
        Ok(RelayMessage::Event {
            subscription_id,
            event,
        }) => {
            // A delivered event is proof that this subscription was accepted.
            if let Some(index) = subscription_rule_index(&subscription_id) {
                health.observe_subscription(index);
            }
            FrameOutcome {
                dispatch: decide_dispatch(config, seen, buckets, now_ms, &subscription_id, &event),
                proven: health.became_proven(now_ms),
                reconnect: false,
            }
        }
        Ok(RelayMessage::Eose { subscription_id }) => {
            // EOSE is the relay saying it accepted this REQ and has finished
            // replaying stored events for it — the only positive acceptance
            // signal NIP-01 offers.
            if let Some(index) = subscription_rule_index(&subscription_id) {
                health.observe_subscription(index);
            }
            FrameOutcome {
                dispatch: None,
                proven: health.became_proven(now_ms),
                reconnect: false,
            }
        }
        Ok(RelayMessage::Closed {
            subscription_id,
            message,
        }) if subscription_rule_index(&subscription_id).is_some() => {
            // The relay can close one subscription (auth change, rejected
            // filter, eviction) while leaving the socket open; sitting on a
            // live connection with a dead subscription would silently stop
            // this rule forever. Reconnect from scratch, matching
            // buzz-waker's taps.
            tracing::warn!(
                subscription = %subscription_id,
                %message,
                "relay closed a rule subscription; reconnecting"
            );
            FrameOutcome {
                dispatch: None,
                proven: false,
                reconnect: true,
            }
        }
        // NOTICE, OK, COUNT, late AUTH, or a CLOSED for a subscription this
        // bridge never opened: nothing to do. Still re-check health, so a
        // relay that never sends EOSE can still prove itself by staying up.
        Ok(_) => FrameOutcome {
            proven: health.became_proven(now_ms),
            ..FrameOutcome::nothing()
        },
        // A quiet subscription is the normal case.
        Err(WsClientError::Timeout) => FrameOutcome {
            proven: health.became_proven(now_ms),
            ..FrameOutcome::nothing()
        },
        Err(error) => {
            tracing::warn!(%error, "relay connection lost; reconnecting");
            FrameOutcome {
                dispatch: None,
                proven: false,
                reconnect: true,
            }
        }
    }
}

/// Run the bridge until `cancel` fires.
///
/// Reconnects on any transport error with the capped exponential ladder
/// ([`reconnect_delay_ms`]); each (re)connect resubscribes every rule with a
/// fresh `since` of now minus [`RESUBSCRIBE_OVERLAP_SECS`]. The ladder only
/// resets once [`ConnectionHealth`] says the relay actually accepted the
/// subscriptions, so a relay that closes them immediately backs off instead
/// of reconnecting twice a second forever.
///
/// # Errors
/// Only client construction can fail; every runtime error is absorbed into
/// the reconnect ladder.
pub async fn run_bridge(config: &BridgeConfig, cancel: &CancellationToken) -> reqwest::Result<()> {
    let client = build_client()?;
    let mut seen = SeenRing::new(SEEN_RING_CAPACITY);
    let mut buckets: Vec<TokenBucket> = config
        .rules
        .iter()
        .map(|rule| TokenBucket::new(rule.max_per_minute, now_ms()))
        .collect();
    let mut ladder = ReconnectLadder::new();

    while !cancel.is_cancelled() {
        let delay_ms = ladder.delay_ms();
        if delay_ms > 0 {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                () = cancel.cancelled() => break,
            }
        }

        let connect = NostrWsConnection::connect_authenticated(
            &config.relay_url,
            &config.keys,
            config.auth_tag.as_ref(),
        );
        let mut connection = tokio::select! {
            result = connect => match result {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::warn!(%error, "relay connect failed; backing off");
                    ladder.connect_failed();
                    continue;
                }
            },
            () = cancel.cancelled() => break,
        };

        let since = now_secs().saturating_sub(RESUBSCRIBE_OVERLAP_SECS);
        let mut subscribed = true;
        for (index, rule) in config.rules.iter().enumerate() {
            if let Err(error) = connection.send_raw(&rule_req(index, rule, since)).await {
                tracing::warn!(rule = %rule.name, %error, "subscribe failed; reconnecting");
                subscribed = false;
                break;
            }
        }
        if !subscribed {
            ladder.connect_failed();
            continue;
        }
        // Deliberately not a reset — see `ReconnectLadder::connected`.
        ladder.connected();
        let mut health = ConnectionHealth::new(config.rules.len(), now_ms());
        tracing::info!(
            rules = config.rules.len(),
            since,
            "connected; subscriptions sent"
        );

        loop {
            let next = tokio::select! {
                result = connection.next_event(Duration::from_secs(IDLE_TIMEOUT_SECS)) => result,
                () = cancel.cancelled() => return Ok(()),
            };

            let mut outcome =
                apply_frame(config, &mut seen, &mut buckets, &mut health, now_ms(), next);

            if let Some(dispatch) = outcome.dispatch.take() {
                // The bucket token was taken synchronously; the HTTP call
                // (with its own timeout and single retry) runs on its own
                // task so a slow webhook never stalls the read loop.
                tokio::spawn(deliver(
                    client.clone(),
                    dispatch.rule_name,
                    dispatch.url_template,
                    dispatch.request,
                ));
            }
            ladder.observe(&outcome);
            if outcome.proven {
                tracing::info!(
                    rules = config.rules.len(),
                    "subscriptions accepted; reconnect backoff reset"
                );
            }
            if outcome.reconnect {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::parse_rules;
    use nostr::{EventBuilder, Keys, Kind, Tag};
    use serde_json::json;
    use std::collections::HashMap;

    /// Two rules that match exactly the same events, differing only in name
    /// and destination — the overlap case a single global dedup key breaks.
    fn overlapping_config(author_hex: &str) -> BridgeConfig {
        let raw = json!([
            {
                "name": "buzz-verdict",
                "filter": { "kinds": [30023], "authors": [author_hex], "d_prefix": "pr-verdict-" },
                "webhook": { "url": "https://one.example.com/dispatch" }
            },
            {
                "name": "verdict-mirror",
                "filter": { "kinds": [30023], "authors": [author_hex], "d_prefix": "pr-verdict-" },
                "webhook": { "url": "https://two.example.com/dispatch" }
            }
        ])
        .to_string();
        BridgeConfig {
            relay_url: "wss://relay.example.com".to_string(),
            keys: Keys::generate(),
            auth_tag: None,
            rules: parse_rules(&raw, &HashMap::new()).expect("parses"),
        }
    }

    fn buckets_for(config: &BridgeConfig) -> Vec<TokenBucket> {
        config
            .rules
            .iter()
            .map(|rule| TokenBucket::new(rule.max_per_minute, 0))
            .collect()
    }

    fn event_frame(event: &nostr::Event, index: usize) -> Result<RelayMessage, WsClientError> {
        Ok(RelayMessage::Event {
            subscription_id: subscription_id(index),
            event: Box::new(event.clone()),
        })
    }

    #[test]
    fn two_overlapping_rules_each_dispatch_exactly_once() {
        let keys = Keys::generate();
        let config = overlapping_config(&keys.public_key().to_hex());
        let event = EventBuilder::new(Kind::Custom(30023), "verdict")
            .tags([Tag::parse(["d", "pr-verdict-yjc801-buzz-121"]).expect("d tag")])
            .sign_with_keys(&keys)
            .expect("sign");

        let mut seen = SeenRing::new(SEEN_RING_CAPACITY);
        let mut buckets = buckets_for(&config);
        let mut health = ConnectionHealth::new(config.rules.len(), 0);

        // The relay delivers the same event once per matching subscription.
        let first = apply_frame(
            &config,
            &mut seen,
            &mut buckets,
            &mut health,
            0,
            event_frame(&event, 0),
        );
        let second = apply_frame(
            &config,
            &mut seen,
            &mut buckets,
            &mut health,
            0,
            event_frame(&event, 1),
        );

        assert_eq!(
            first.dispatch.map(|dispatch| dispatch.rule_name).as_deref(),
            Some("buzz-verdict")
        );
        assert_eq!(
            second
                .dispatch
                .map(|dispatch| dispatch.rule_name)
                .as_deref(),
            Some("verdict-mirror"),
            "the second rule must fire too — a dedup key of the event id alone would have let \
             the first delivery suppress this webhook entirely"
        );

        // …and each rule still deduplicates its own replays.
        for index in 0..2 {
            assert!(
                apply_frame(
                    &config,
                    &mut seen,
                    &mut buckets,
                    &mut health,
                    0,
                    event_frame(&event, index),
                )
                .dispatch
                .is_none(),
                "rule {index} must not fire twice for one event"
            );
        }
    }

    #[test]
    fn an_unverifiable_event_never_dispatches() {
        let keys = Keys::generate();
        let config = overlapping_config(&keys.public_key().to_hex());
        let mut event = EventBuilder::new(Kind::Custom(30023), "verdict")
            .tags([Tag::parse(["d", "pr-verdict-yjc801-buzz-121"]).expect("d tag")])
            .sign_with_keys(&keys)
            .expect("sign");
        // A hostile relay hands us a frame whose content no longer matches its
        // signed id.
        event.content = "tampered".to_string();

        let mut seen = SeenRing::new(SEEN_RING_CAPACITY);
        let mut buckets = buckets_for(&config);
        let mut health = ConnectionHealth::new(config.rules.len(), 0);
        assert!(apply_frame(
            &config,
            &mut seen,
            &mut buckets,
            &mut health,
            0,
            event_frame(&event, 0),
        )
        .dispatch
        .is_none());
    }

    #[test]
    fn a_relay_that_closes_every_subscription_advances_the_backoff_ladder() {
        let config = overlapping_config(&"ab".repeat(32));
        let mut seen = SeenRing::new(SEEN_RING_CAPACITY);
        let mut buckets = buckets_for(&config);

        let mut ladder = ReconnectLadder::new();
        let mut delays = Vec::new();
        // Five reconnect attempts against a relay that accepts the socket,
        // takes the REQ, and immediately CLOSEs it (subscription cap, policy
        // change, a filter it will not serve). This is the loop's real
        // per-attempt sequence: wait, connect, subscribe, read one frame.
        for _ in 0..5 {
            delays.push(ladder.delay_ms());
            ladder.connected(); // the socket is up and every REQ was written
            let mut health = ConnectionHealth::new(config.rules.len(), 0);
            let outcome = apply_frame(
                &config,
                &mut seen,
                &mut buckets,
                &mut health,
                0,
                Ok(RelayMessage::Closed {
                    subscription_id: subscription_id(0),
                    message: "subscription limit reached".to_string(),
                }),
            );
            assert!(outcome.reconnect, "a CLOSED ends the connection");
            assert!(
                !outcome.proven,
                "sending the REQ is not acceptance; nothing here proves the subscription healthy"
            );
            ladder.observe(&outcome);
        }

        assert_eq!(
            delays,
            vec![0, 500, 1_000, 2_000, 4_000],
            "the ladder must climb. Clearing it in `connected()` — on nothing better than a \
             successful send_raw — pinned it at the bottom rung and hammered the relay at two \
             authenticated connections per second, forever"
        );
    }

    #[test]
    fn the_ladder_resets_only_once_every_rule_subscription_is_accepted() {
        let config = overlapping_config(&"ab".repeat(32));
        let mut seen = SeenRing::new(SEEN_RING_CAPACITY);
        let mut buckets = buckets_for(&config);
        let mut health = ConnectionHealth::new(config.rules.len(), 0);

        let eose = |index: usize| {
            Ok(RelayMessage::Eose {
                subscription_id: subscription_id(index),
            })
        };

        // Start three failures deep, as a bridge reconnecting into a bad
        // relay would be.
        let mut ladder = ReconnectLadder::new();
        for _ in 0..3 {
            ladder.connect_failed();
        }
        let deep = ladder.delay_ms();
        assert_eq!(deep, RECONNECT_BASE_DELAY_MS * 4);

        let first = apply_frame(&config, &mut seen, &mut buckets, &mut health, 0, eose(0));
        assert!(
            !first.proven,
            "one of two rules accepted is not a healthy connection"
        );
        ladder.observe(&first);
        assert_eq!(ladder.delay_ms(), deep, "the failure generation survives");

        let second = apply_frame(&config, &mut seen, &mut buckets, &mut health, 0, eose(1));
        assert!(
            second.proven,
            "every rule accepted; the connection is proven"
        );
        ladder.observe(&second);
        assert_eq!(
            ladder.delay_ms(),
            0,
            "a proven connection clears the ladder"
        );

        let third = apply_frame(&config, &mut seen, &mut buckets, &mut health, 0, eose(1));
        assert!(
            !third.proven,
            "proof is reported once, at the transition, not on every later frame"
        );
    }

    #[test]
    fn a_long_lived_connection_proves_itself_without_eose() {
        // A relay that answers REQs but never sends EOSE would otherwise never
        // clear the ladder, so every later reconnect would start at the 30s
        // ceiling. Surviving well past an immediate CLOSED is enough.
        let mut health = ConnectionHealth::new(2, 1_000);
        assert!(!health.became_proven(1_000));
        assert!(
            !health.became_proven(1_000 + STABLE_CONNECTION_MS - 1),
            "just short of the threshold is not proof"
        );
        assert!(health.became_proven(1_000 + STABLE_CONNECTION_MS));
        assert!(!health.became_proven(u64::MAX), "reported once only");
    }

    #[test]
    fn a_delivered_event_also_proves_its_subscription() {
        let mut health = ConnectionHealth::new(2, 0);
        health.observe_subscription(0);
        assert!(!health.became_proven(0));
        health.observe_subscription(1);
        assert!(health.became_proven(0));
    }

    #[test]
    fn the_backoff_ladder_doubles_and_caps() {
        assert_eq!(reconnect_delay_ms(0), 0, "no failures, no wait");
        assert_eq!(reconnect_delay_ms(1), RECONNECT_BASE_DELAY_MS);
        assert_eq!(reconnect_delay_ms(2), RECONNECT_BASE_DELAY_MS * 2);
        assert_eq!(reconnect_delay_ms(3), RECONNECT_BASE_DELAY_MS * 4);
        assert_eq!(reconnect_delay_ms(10), RECONNECT_MAX_DELAY_MS);
        assert_eq!(reconnect_delay_ms(u32::MAX), RECONNECT_MAX_DELAY_MS);
    }

    #[test]
    fn subscription_ids_round_trip_and_foreign_ids_do_not_parse() {
        assert_eq!(subscription_rule_index(&subscription_id(0)), Some(0));
        assert_eq!(subscription_rule_index(&subscription_id(17)), Some(17));
        assert_eq!(subscription_rule_index("bridge-"), None);
        assert_eq!(subscription_rule_index("bridge-x"), None);
        assert_eq!(subscription_rule_index("something-else"), None);
    }

    #[test]
    fn the_req_carries_kinds_authors_and_since_but_never_d_prefix() {
        let raw = json!([{
            "name": "req-shape",
            "filter": {
                "kinds": [30023],
                "authors": ["ab".repeat(32)],
                "d_prefix": "pr-verdict-"
            },
            "webhook": { "url": "https://example.com" }
        }])
        .to_string();
        let rule = &parse_rules(&raw, &HashMap::new()).expect("parses")[0];

        let req = rule_req(3, rule, 1_700_000_000);
        assert_eq!(req[0], "REQ");
        assert_eq!(req[1], "bridge-3");
        assert_eq!(req[2]["kinds"], json!([30023]));
        assert_eq!(req[2]["authors"], json!(["ab".repeat(32)]));
        assert_eq!(req[2]["since"], 1_700_000_000_u64);
        assert!(
            req[2].get("#d").is_none(),
            "d_prefix is client-side; the relay's #d filter is exact-match only"
        );
    }
}
