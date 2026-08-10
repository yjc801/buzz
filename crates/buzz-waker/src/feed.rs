//! The event feed — §4 of `PLANS/BUZZ_WAKER_DESIGN.md`.
//!
//! Everything before this module decides things; this is what gives it
//! something to decide about. It holds one relay subscription for the mentions
//! that address a watched agent, folds each one through the durable cursor, and
//! survives the connection dropping without losing the events that arrived
//! while it was gone.
//!
//! # Shape
//!
//! The same split as [`crate::attempt`]: the policy is pure and the socket is
//! behind [`FeedTransport`]. So the parts that are easy to get wrong — the
//! filter, the reconnect ladder, what each relay frame means — are tested
//! directly, and the transport is a thin adapter with nothing to reason about.
//!
//! # Two things that are not obvious
//!
//! - **The REQ must carry `kinds`.** A filter without them trips the relay's
//!   p-gate and comes back 403 (`CLAUDE.md` § Common Gotchas), which reads as a
//!   permission problem rather than a malformed query. [`WAKE_TRIGGER_KINDS`]
//!   is the right set anyway: [`crate::decide`] discards everything else, so
//!   asking for less traffic costs nothing.
//! - **The waker connects interactive**, sending no `class` tag. The read-only
//!   connection class exists in this repo's relay but is not deployed on the
//!   relay we use, and the client fails closed when a requested class is not
//!   confirmed — so requesting it would refuse every connection. §4's
//!   deployment amendment records the consequence: a cleanly-exiting watched
//!   agent's presence dot lingers for up to its 180s TTL instead of clearing
//!   at once. Bounded, and the bound holds because a watcher never publishes
//!   presence and so never refreshes that TTL.

use serde_json::{json, Value};

use crate::cursor::{Admission, CursorStore};
use crate::decide::{normalize_pubkey, TriggerEvent, WAKE_TRIGGER_KINDS};

/// Subscription id for the wake feed.
///
/// Fixed rather than random: re-issuing a REQ with the same id after a
/// reconnect replaces the old subscription instead of accumulating one per
/// attempt, and a fixed id keeps relay-side logs legible across restarts.
pub const WAKE_SUBSCRIPTION_ID: &str = "buzz-waker-wake";

/// How long to wait for a frame before treating the connection as idle.
///
/// Idle is not failure — a quiet channel is the normal case. It is the cue to
/// checkpoint coverage ([`CursorStore::mark_covered`]) so a later restart can
/// tell "nothing happened" from "we were not watching".
pub const FEED_IDLE_TIMEOUT_SECS: u64 = 60;

/// First rung of the reconnect ladder.
pub const RECONNECT_BASE_DELAY_MS: u64 = 500;

/// Ceiling on the reconnect ladder.
///
/// A waker that has been failing for an hour should still retry every 30s: the
/// cost of a retry is one WebSocket handshake, and the cost of backing off
/// further is a mention nobody answers.
pub const RECONNECT_MAX_DELAY_MS: u64 = 30_000;

/// What a relay frame means to the feed.
///
/// Deliberately smaller than the relay's message set: the feed reacts to four
/// things and ignores the rest, and naming the ignored ones here rather than
/// matching them at the call site keeps the loop honest about what it drops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedSignal {
    /// An event on our subscription that parsed into a candidate trigger.
    Trigger(Box<TriggerEvent>),
    /// Stored events are drained; the subscription is now live.
    ReplayComplete,
    /// The relay closed our subscription. Reconnect; do not treat as idle.
    Closed(String),
    /// Nothing the feed acts on.
    Ignored,
}

/// A relay frame, reduced to what the feed reads.
///
/// The feed defines its own frame type rather than matching on the ws client's
/// so that [`classify`] — the part with the subscription-id and parsing rules
/// worth testing — needs no socket, no runtime, and no relay to exercise.
#[derive(Debug, Clone)]
pub enum FeedFrame {
    /// An event delivered on some subscription.
    Event {
        /// The subscription the relay attributed it to.
        subscription_id: String,
        /// The raw event JSON.
        event: Value,
    },
    /// End of stored events for a subscription.
    Eose {
        /// The subscription that finished its replay.
        subscription_id: String,
    },
    /// The relay closed a subscription.
    Closed {
        /// The subscription that was closed.
        subscription_id: String,
        /// The relay's stated reason.
        message: String,
    },
    /// Anything else — NOTICE, OK, COUNT, a late AUTH challenge.
    Other,
}

/// The REQ filter for the wake feed.
///
/// `#p` is the addressing the harness itself keys on, so it is what the feed
/// asks for; `kinds` is mandatory (see the module note); `since` comes from
/// [`CursorStore::resume`], which has already subtracted the reconnect overlap.
///
/// Watched pubkeys are normalized and de-duplicated. A duplicate would be
/// harmless to the relay but makes the filter's size a poor proxy for how many
/// agents are actually watched, which is the number an operator reads.
#[must_use]
pub fn wake_filter(watched: &[String], since: u64) -> Value {
    let mut seen = std::collections::BTreeSet::new();
    let targets: Vec<String> = watched
        .iter()
        .map(|pubkey| normalize_pubkey(pubkey))
        .filter(|pubkey| !pubkey.is_empty() && seen.insert(pubkey.clone()))
        .collect();

    json!({
        "kinds": WAKE_TRIGGER_KINDS,
        "#p": targets,
        "since": since,
    })
}

/// The full REQ frame for the wake feed.
#[must_use]
pub fn wake_req(watched: &[String], since: u64) -> Value {
    json!(["REQ", WAKE_SUBSCRIPTION_ID, wake_filter(watched, since)])
}

/// Read a relay event into the fields a wake decision needs.
///
/// Returns `None` for anything structurally unusable — a missing id, author or
/// `created_at`, or a kind outside `u32`. Refusing beats defaulting: every
/// field here feeds a decision that spends a real deploy, and an event with an
/// invented author would be evaluated against the wrong respond-to policy.
///
/// Only `p` tags are read. `h`, `e` and the rest are deliberately dropped —
/// the decision does not use them, and not copying them keeps the feed's
/// in-memory footprint to the fields §4's R4 names.
#[must_use]
pub fn trigger_event_from_json(event: &Value) -> Option<Box<TriggerEvent>> {
    let id = event.get("id")?.as_str()?.trim();
    let author = event.get("pubkey")?.as_str()?.trim();
    if id.is_empty() || author.is_empty() {
        return None;
    }
    let kind = u32::try_from(event.get("kind")?.as_u64()?).ok()?;
    let created_at = event.get("created_at")?.as_u64()?;

    let p_tags = event
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(|tag| {
                    let parts = tag.as_array()?;
                    if parts.first()?.as_str()? != "p" {
                        return None;
                    }
                    Some(parts.get(1)?.as_str()?.to_string())
                })
                .collect()
        })
        .unwrap_or_default();

    Some(Box::new(TriggerEvent {
        id: id.to_string(),
        author: author.to_string(),
        kind,
        p_tags,
        created_at,
    }))
}

/// What a frame means to the feed.
///
/// Frames for another subscription id are [`FeedSignal::Ignored`], not an
/// error: one connection may legitimately carry more than one subscription,
/// and silently acting on another's events would wake agents from a filter
/// this module never wrote.
#[must_use]
pub fn classify(frame: &FeedFrame) -> FeedSignal {
    match frame {
        FeedFrame::Event {
            subscription_id,
            event,
        } if subscription_id == WAKE_SUBSCRIPTION_ID => trigger_event_from_json(event)
            .map(FeedSignal::Trigger)
            .unwrap_or(FeedSignal::Ignored),
        FeedFrame::Eose { subscription_id } if subscription_id == WAKE_SUBSCRIPTION_ID => {
            FeedSignal::ReplayComplete
        }
        FeedFrame::Closed {
            subscription_id,
            message,
        } if subscription_id == WAKE_SUBSCRIPTION_ID => FeedSignal::Closed(message.clone()),
        _ => FeedSignal::Ignored,
    }
}

/// Delay before the next connection attempt, in milliseconds.
///
/// Doubling from [`RECONNECT_BASE_DELAY_MS`], capped at
/// [`RECONNECT_MAX_DELAY_MS`]. `consecutive_failures` is the count *before*
/// this attempt, so the first retry after one failure waits the base delay.
///
/// No jitter, deliberately: jitter exists to de-synchronise a fleet, and there
/// is one waker per owner. Adding it here would only make the ladder
/// untestable.
#[must_use]
pub fn reconnect_delay_ms(consecutive_failures: u32) -> u64 {
    if consecutive_failures == 0 {
        return 0;
    }
    RECONNECT_BASE_DELAY_MS
        .saturating_mul(1_u64 << consecutive_failures.saturating_sub(1).min(16))
        .min(RECONNECT_MAX_DELAY_MS)
}

/// The socket, behind a trait so the loop above it can be tested.
///
/// One connection, owned for the transport's lifetime. `connect` is separate
/// from construction because a reconnect must be able to rebuild the socket
/// without rebuilding the caller's state — the cursor and the in-flight set
/// have to outlive any one connection.
pub trait FeedTransport {
    /// Errors this transport can raise.
    type Error;

    /// Establish (or re-establish) the connection and authenticate.
    fn connect(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>>;

    /// Send a raw frame — in practice the REQ from [`wake_req`].
    fn send(&mut self, frame: &Value)
        -> impl std::future::Future<Output = Result<(), Self::Error>>;

    /// Next frame, or `None` if `timeout_secs` passed with the connection
    /// still healthy. A dropped connection is an `Err`, not `None`: the
    /// difference decides between checkpointing coverage and reconnecting.
    fn next_frame(
        &mut self,
        timeout_secs: u64,
    ) -> impl std::future::Future<Output = Result<Option<FeedFrame>, Self::Error>>;
}

/// What the feed did with one frame, for the caller to act on.
///
/// The feed does not itself wake anything: [`crate::attempt::run_wake_attempt`]
/// needs effects this module has no business holding. It hands over admitted
/// triggers and lets the daemon own the deploy.
#[derive(Debug)]
pub enum FeedStep {
    /// A fresh trigger, already claimed in the cursor. The caller must reach a
    /// terminal outcome and then call [`CursorStore::complete`] — or
    /// [`CursorStore::abandon`] — or the checkpoint stays pinned to it.
    Admitted {
        /// The event to decide on.
        event: Box<TriggerEvent>,
        /// Set when the event was already older than a woken harness's replay
        /// window at admission. Process it anyway, but report the attempt as a
        /// failure rather than a healthy wake — see
        /// [`Admission::FreshButUndeliverable`].
        undeliverable: bool,
    },
    /// A duplicate, or a frame the feed ignores. Nothing to do.
    Nothing,
    /// The subscription is live; stored events are drained.
    ReplayComplete,
    /// The relay closed the subscription. The caller should reconnect.
    Closed(String),
}

/// Fold one frame into the cursor and say what the caller should do with it.
///
/// Kept separate from the connection loop so the interesting half — what
/// happens to the cursor for each kind of frame — is exercised without a
/// socket.
///
/// # Errors
/// Propagates [`crate::cursor::CursorError`] from the durable claim. A cursor
/// that cannot be made durable is fatal by design: continuing would process
/// events the next restart has no record of.
pub fn step(
    cursor: &mut CursorStore,
    frame: &FeedFrame,
    now: u64,
) -> Result<FeedStep, crate::cursor::CursorError> {
    match classify(frame) {
        FeedSignal::Trigger(event) => {
            let admission = cursor.admit(&event.id, event.created_at, now)?;
            match admission {
                Admission::Fresh => Ok(FeedStep::Admitted {
                    event,
                    undeliverable: false,
                }),
                Admission::FreshButUndeliverable { .. } => Ok(FeedStep::Admitted {
                    event,
                    undeliverable: true,
                }),
                Admission::Duplicate => Ok(FeedStep::Nothing),
            }
        }
        FeedSignal::ReplayComplete => {
            cursor.end_replay(now)?;
            Ok(FeedStep::ReplayComplete)
        }
        FeedSignal::Closed(reason) => Ok(FeedStep::Closed(reason)),
        FeedSignal::Ignored => Ok(FeedStep::Nothing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_core::kind::{KIND_REACTION, KIND_STREAM_MESSAGE};

    fn event_json(id: &str, kind: u32, p_tags: &[&str]) -> Value {
        json!({
            "id": id,
            "pubkey": "aa".repeat(32),
            "kind": kind,
            "created_at": 1_700_000_000_u64,
            "tags": p_tags
                .iter()
                .map(|p| json!(["p", p]))
                .collect::<Vec<_>>(),
            "content": "hello",
        })
    }

    #[test]
    fn the_filter_always_names_kinds() {
        // Without `kinds` the relay's p-gate answers 403, which reads as an
        // auth failure rather than a bad filter. This is the guard for that.
        let filter = wake_filter(&["AB".repeat(32)], 42);
        let kinds = filter["kinds"].as_array().expect("kinds must be present");
        assert_eq!(kinds.len(), WAKE_TRIGGER_KINDS.len());
        assert_eq!(filter["since"], 42);
    }

    #[test]
    fn watched_pubkeys_are_normalized_and_deduplicated() {
        let filter = wake_filter(
            &[
                "AB".repeat(32),
                format!("  {}  ", "ab".repeat(32)),
                "cd".repeat(32),
                String::new(),
            ],
            0,
        );
        let targets = filter["#p"].as_array().expect("#p must be present");
        assert_eq!(
            targets.len(),
            2,
            "the same key in three spellings is one target, and blank is none"
        );
        assert_eq!(targets[0], "ab".repeat(32));
    }

    #[test]
    fn an_event_on_another_subscription_is_ignored() {
        // One connection can carry more than one subscription. Acting on a
        // frame from a filter this module never wrote would wake agents for
        // reasons no test here covers.
        let frame = FeedFrame::Event {
            subscription_id: "someone-elses".to_string(),
            event: event_json("ff".repeat(32).as_str(), KIND_STREAM_MESSAGE, &[]),
        };
        assert_eq!(classify(&frame), FeedSignal::Ignored);
    }

    #[test]
    fn a_structurally_broken_event_is_ignored_not_defaulted() {
        for missing in ["id", "pubkey", "kind", "created_at"] {
            let mut event = event_json(&"ff".repeat(32), KIND_STREAM_MESSAGE, &[]);
            event.as_object_mut().expect("object").remove(missing);
            assert!(
                trigger_event_from_json(&event).is_none(),
                "an event missing {missing} must be refused, never defaulted"
            );
        }
    }

    #[test]
    fn only_p_tags_are_carried_off_the_wire() {
        let event = json!({
            "id": "ff".repeat(32),
            "pubkey": "aa".repeat(32),
            "kind": KIND_STREAM_MESSAGE,
            "created_at": 1_700_000_000_u64,
            "tags": [
                ["h", "channel-uuid"],
                ["p", "bb".repeat(32)],
                ["e", "cc".repeat(32), "", "reply"],
                ["p", "dd".repeat(32)],
            ],
        });
        let trigger = trigger_event_from_json(&event).expect("well-formed event");
        assert_eq!(trigger.p_tags, vec!["bb".repeat(32), "dd".repeat(32)]);
    }

    #[test]
    fn a_non_trigger_kind_still_parses_and_is_refused_downstream() {
        // The feed does not filter by kind a second time — `select_wake_candidates`
        // owns that gate. Parsing here must not silently drop the event, or the
        // reason it was refused would be invisible.
        let event = event_json(&"ff".repeat(32), KIND_REACTION, &[]);
        let trigger = trigger_event_from_json(&event).expect("parses");
        assert_eq!(trigger.kind, KIND_REACTION);
    }

    fn cursor_at(dir: &tempfile::TempDir, now: u64) -> CursorStore {
        CursorStore::open_or_start(
            dir.path().join("cursor.json"),
            now,
            crate::cursor::DEFAULT_COMPLETED_RING,
        )
        .expect("a fresh cursor opens")
    }

    #[test]
    fn a_replayed_duplicate_is_claimed_once() {
        // The reconnect overlap re-delivers events on purpose, so the same
        // mention arriving twice is the normal case, not an anomaly. Waking
        // twice for it would spend a second deploy on an agent that is already
        // being started.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 1_700_000_000;
        let mut cursor = cursor_at(&dir, now);
        let frame = FeedFrame::Event {
            subscription_id: WAKE_SUBSCRIPTION_ID.to_string(),
            event: event_json(
                &"ff".repeat(32),
                KIND_STREAM_MESSAGE,
                &["bb".repeat(32).as_str()],
            ),
        };

        let first = step(&mut cursor, &frame, now).expect("first admission");
        assert!(
            matches!(first, FeedStep::Admitted { .. }),
            "the first delivery is the one that does the work"
        );
        let second = step(&mut cursor, &frame, now).expect("second admission");
        assert!(
            matches!(second, FeedStep::Nothing),
            "the same event delivered again must not be claimed twice"
        );
    }

    #[test]
    fn eose_ends_the_replay_and_advances_coverage() {
        // EOSE is the only proof that the *stored* backlog is drained, which is
        // what lets the checkpoint stop being pinned to the replay floor.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 1_700_000_000;
        let mut cursor = cursor_at(&dir, now);
        // The pin only exists while a replay is draining, and `resume` is what
        // sets it — the loop's own order: resume, REQ, then EOSE.
        let _ = cursor.resume(now);
        let before = cursor.state().covered_through_secs;

        let step_result = step(
            &mut cursor,
            &FeedFrame::Eose {
                subscription_id: WAKE_SUBSCRIPTION_ID.to_string(),
            },
            now + 30,
        )
        .expect("end_replay persists");

        assert!(matches!(step_result, FeedStep::ReplayComplete));
        assert!(
            cursor.state().covered_through_secs > before,
            "EOSE is evidence the feed is current, so coverage must advance"
        );
    }

    #[test]
    fn a_closed_subscription_is_not_mistaken_for_a_quiet_one() {
        // CLOSED and idle look alike from the loop's perspective and must not:
        // one means reconnect, the other means checkpoint and keep waiting.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 1_700_000_000;
        let mut cursor = cursor_at(&dir, now);

        let step_result = step(
            &mut cursor,
            &FeedFrame::Closed {
                subscription_id: WAKE_SUBSCRIPTION_ID.to_string(),
                message: "restricted: not authorized".to_string(),
            },
            now,
        )
        .expect("classifying a close touches no durable state");

        match step_result {
            FeedStep::Closed(reason) => assert!(reason.contains("restricted")),
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    #[test]
    fn the_reconnect_ladder_climbs_then_holds() {
        assert_eq!(reconnect_delay_ms(0), 0, "no failures, no wait");
        assert_eq!(reconnect_delay_ms(1), RECONNECT_BASE_DELAY_MS);
        assert_eq!(reconnect_delay_ms(2), RECONNECT_BASE_DELAY_MS * 2);
        assert_eq!(reconnect_delay_ms(3), RECONNECT_BASE_DELAY_MS * 4);
        assert_eq!(reconnect_delay_ms(30), RECONNECT_MAX_DELAY_MS);
        assert_eq!(
            reconnect_delay_ms(u32::MAX),
            RECONNECT_MAX_DELAY_MS,
            "the ladder must not overflow into a short delay"
        );
    }
}
