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

/// Handle one delivered event frame: verify, match, dedup, budget, dispatch.
fn handle_event(
    config: &BridgeConfig,
    client: &reqwest::Client,
    seen: &mut SeenRing,
    buckets: &mut [TokenBucket],
    subscription: &str,
    event: &nostr::Event,
) {
    let Some(rule_index) = subscription_rule_index(subscription) else {
        return; // not a subscription this bridge opened
    };
    let Some(rule) = config.rules.get(rule_index) else {
        return;
    };

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
        return;
    }

    let fields = EventFields::from_event(event);
    if !rule_matches(rule, &fields) {
        // The relay-side filter cannot express d_prefix, so a non-match here
        // is routine, not suspicious.
        return;
    }

    if !seen.first_sighting(&fields.id) {
        tracing::debug!(rule = %rule.name, event_id = %fields.id, "duplicate delivery suppressed");
        return;
    }

    let Some(bucket) = buckets.get_mut(rule_index) else {
        return;
    };
    if !bucket.try_take(now_ms()) {
        tracing::warn!(
            rule = %rule.name,
            event_id = %fields.id,
            max_per_minute = rule.max_per_minute,
            "over the rule's dispatch budget; dropping this match"
        );
        return;
    }

    // The bucket token was taken synchronously; the HTTP call (with its own
    // timeout and single retry) runs on its own task so a slow webhook never
    // stalls the read loop.
    let request = PreparedRequest::build(rule, &fields);
    tracing::info!(
        rule = %rule.name,
        event_id = %fields.id,
        kind = fields.kind,
        "rule matched; dispatching webhook"
    );
    tokio::spawn(deliver(
        client.clone(),
        rule.name.clone(),
        rule.webhook.url.template().to_string(),
        request,
    ));
}

/// Run the bridge until `cancel` fires.
///
/// Reconnects on any transport error with the capped exponential ladder
/// ([`reconnect_delay_ms`]); each (re)connect resubscribes every rule with a
/// fresh `since` of now minus [`RESUBSCRIBE_OVERLAP_SECS`].
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
    let mut consecutive_failures = 0u32;

    while !cancel.is_cancelled() {
        if consecutive_failures > 0 {
            let delay = Duration::from_millis(reconnect_delay_ms(consecutive_failures));
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
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
                    consecutive_failures = consecutive_failures.saturating_add(1);
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
            consecutive_failures = consecutive_failures.saturating_add(1);
            continue;
        }
        consecutive_failures = 0;
        tracing::info!(
            rules = config.rules.len(),
            since,
            "connected and subscribed"
        );

        loop {
            let next = tokio::select! {
                result = connection.next_event(Duration::from_secs(IDLE_TIMEOUT_SECS)) => result,
                () = cancel.cancelled() => return Ok(()),
            };

            match next {
                Ok(RelayMessage::Event {
                    subscription_id,
                    event,
                }) => {
                    handle_event(
                        config,
                        &client,
                        &mut seen,
                        &mut buckets,
                        &subscription_id,
                        &event,
                    );
                }
                Ok(RelayMessage::Closed {
                    subscription_id,
                    message,
                }) if subscription_rule_index(&subscription_id).is_some() => {
                    // The relay can close one subscription (auth change,
                    // rejected filter, eviction) while leaving the socket
                    // open; sitting on a live connection with a dead
                    // subscription would silently stop this rule forever.
                    // Reconnect from scratch, matching buzz-waker's taps.
                    tracing::warn!(
                        subscription = %subscription_id,
                        %message,
                        "relay closed a rule subscription; reconnecting"
                    );
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    break;
                }
                // EOSE, NOTICE, OK, COUNT, late AUTH, or a CLOSED for a
                // subscription this bridge never opened: nothing to do.
                Ok(_) => {}
                Err(WsClientError::Timeout) => {
                    // A quiet subscription is the normal case.
                }
                Err(error) => {
                    tracing::warn!(%error, "relay connection lost; reconnecting");
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    break;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::parse_rules;
    use serde_json::json;
    use std::collections::HashMap;

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
