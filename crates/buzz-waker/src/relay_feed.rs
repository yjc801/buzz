//! The relay-backed [`FeedTransport`] — the only part of the feed that owns a
//! socket.
//!
//! Deliberately thin. Everything with a rule in it lives in [`crate::feed`] and
//! is tested there; what remains here is connect, send, receive, signature
//! verification, and the translation from the ws client's message set into
//! [`FeedFrame`]. If this file starts making *wake* decisions, they belong
//! upstairs — but authenticating the wire is the transport's own job, and it
//! has to happen here because this is the last point at which the signature
//! still exists.
//!
//! # Why the signature is checked here
//!
//! [`buzz_ws_client`] deserializes an EVENT frame into a [`nostr::Event`] and
//! stops there — JSON shape, no cryptography. [`crate::feed`] then reads the
//! author out of the projected JSON and [`crate::decide`] evaluates it against
//! that agent's respond-to policy. So without a check on this line, a hostile
//! or compromised relay can hand us a structurally perfect event claiming the
//! owner as its author and spend a real deploy on it. The signature is dropped
//! during projection, so nothing downstream can recover the ability to tell.

use std::time::Duration;

use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};
use nostr::{Keys, Tag};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::feed::{
    wake_backfill_close, wake_backfill_req, wake_live_close, wake_live_req, wake_membership_req,
    FeedFrame, FeedTransport,
};

/// Subscription id for the one-shot channel-discovery query
/// ([`RelayFeed::discover_channels`]) — kind:39002 (NIP-29 group members)
/// filtered to `#p` = this agent. Never appears in [`WakeSubscription`],
/// because discovery runs to completion before the feed's main loop ever
/// calls [`FeedTransport::next_frame`] for real, so no other code needs to
/// recognise it.
///
/// [`WakeSubscription`]: crate::feed::WakeSubscription
const DISCOVER_CHANNELS_SUBSCRIPTION_ID: &str = "buzz-waker-discover";

/// How long to wait for the discovery query's EOSE before giving up.
///
/// A relay that never answers would otherwise hang connection setup
/// indefinitely; the reconnect ladder is what actually recovers from this,
/// so failing the connection attempt here is the correct behaviour, not a
/// missing feature.
const DISCOVER_CHANNELS_TIMEOUT_SECS: u64 = 20;

/// A NIP-42-authenticated connection to one relay, **as one agent**,
/// reconnectable in place.
///
/// Holds the credentials rather than a live socket, because a reconnect has to
/// rebuild the socket while the caller's cursor and in-flight set survive.
///
/// One of these per watched agent. The relay authorizes both historical REQ
/// visibility and live private-channel fan-out against the connection's own
/// pubkey, so a single connection cannot serve a watch list — see
/// [`crate::feed::wake_filter`]. Every subscribe method builds its filter from
/// `keys`, which is what stops the two from ever disagreeing.
pub struct RelayFeed {
    relay_url: String,
    keys: Keys,
    agent_pubkey: String,
    auth_tag: Option<Tag>,
    connection: Option<NostrWsConnection>,
}

impl RelayFeed {
    /// Build a feed transport for `relay_url`, authenticating as `keys`.
    ///
    /// `auth_tag` is the NIP-OA attestation, when the identity has one.
    ///
    /// **No connection class is requested.** The waker connects interactive,
    /// which is what §4's deployment amendment settled: the read-only class
    /// exists in this repo's relay but is not deployed on the relay we use, and
    /// [`NostrWsConnection::authenticate_with_tags`] fails closed when a
    /// requested class is not confirmed — so asking for it would refuse every
    /// connection rather than degrade quietly. The cost of not asking is a
    /// bounded one, recorded on [`crate::feed`].
    #[must_use]
    pub fn new(relay_url: impl Into<String>, keys: Keys, auth_tag: Option<Tag>) -> Self {
        let agent_pubkey = keys.public_key().to_hex();
        Self {
            relay_url: relay_url.into(),
            keys,
            agent_pubkey,
            auth_tag,
            connection: None,
        }
    }

    fn connection_mut(&mut self) -> Result<&mut NostrWsConnection, WsClientError> {
        self.connection
            .as_mut()
            .ok_or(WsClientError::ConnectionClosed)
    }
}

impl FeedTransport for RelayFeed {
    type Error = WsClientError;

    fn agent_pubkey(&self) -> &str {
        &self.agent_pubkey
    }

    async fn connect(&mut self) -> Result<(), Self::Error> {
        // Drop any previous socket before dialling: holding both would leave a
        // second authenticated connection for this pubkey open on the relay,
        // and presence is cleared only when the *last* one goes.
        self.connection = None;
        self.connection = Some(
            NostrWsConnection::connect_authenticated(
                &self.relay_url,
                &self.keys,
                self.auth_tag.as_ref(),
            )
            .await?,
        );
        Ok(())
    }

    async fn discover_channels(&mut self) -> Result<Vec<Uuid>, Self::Error> {
        // `self.agent_pubkey`, never an argument — same reasoning as every
        // other subscribe method: the query's `#p` and the socket's
        // authenticated identity must be the same value by construction, or
        // this could discover a channel list that does not belong to the
        // connection that will go on to subscribe live to it.
        let filter = json!({
            "kinds": [buzz_core::kind::KIND_NIP29_GROUP_MEMBERS],
            "#p": [self.agent_pubkey],
        });
        let req = json!(["REQ", DISCOVER_CHANNELS_SUBSCRIPTION_ID, filter]);
        self.connection_mut()?.send_raw(&req).await?;

        let mut channel_ids = Vec::new();
        loop {
            match self.next_frame(DISCOVER_CHANNELS_TIMEOUT_SECS).await? {
                Some(FeedFrame::Event {
                    subscription_id,
                    event,
                }) if subscription_id == DISCOVER_CHANNELS_SUBSCRIPTION_ID => {
                    if let Some(channel_id) = extract_d_tag_uuid(&event) {
                        channel_ids.push(channel_id);
                    }
                }
                Some(FeedFrame::Eose { subscription_id })
                    if subscription_id == DISCOVER_CHANNELS_SUBSCRIPTION_ID =>
                {
                    break;
                }
                // A frame for anything else cannot arrive here — no other
                // subscription is open yet at this point in the connect
                // sequence — but ignoring rather than erroring costs nothing
                // and keeps this loop from being the one place a stray relay
                // message tears down the connection.
                Some(_) => {}
                None => return Err(WsClientError::Timeout),
            }
        }
        Ok(channel_ids)
    }

    async fn subscribe_membership(&mut self, since: u64) -> Result<(), Self::Error> {
        let req = wake_membership_req(&self.agent_pubkey, since);
        self.connection_mut()?.send_raw(&req).await
    }

    async fn subscribe_channel_live(
        &mut self,
        channel_id: Uuid,
        since: u64,
    ) -> Result<(), Self::Error> {
        // `self.agent_pubkey`, never an argument: the filter's `#p` and the
        // socket's authenticated identity are the same value by construction.
        let req = wake_live_req(&self.agent_pubkey, channel_id, since);
        self.connection_mut()?.send_raw(&req).await
    }

    async fn unsubscribe_channel_live(&mut self, channel_id: Uuid) -> Result<(), Self::Error> {
        self.connection_mut()?
            .send_raw(&wake_live_close(channel_id))
            .await
    }

    async fn subscribe_backfill(
        &mut self,
        since: u64,
        until: Option<u64>,
    ) -> Result<(), Self::Error> {
        let req = wake_backfill_req(&self.agent_pubkey, since, until);
        self.connection_mut()?.send_raw(&req).await
    }

    async fn close_backfill(&mut self) -> Result<(), Self::Error> {
        // Unconditional and harmless: CLOSE for an id the relay has no
        // subscription under is answered with a CLOSED the feed ignores, which
        // is why the caller can send this on every completion without tracking
        // whether a backfill was ever opened.
        self.connection_mut()?
            .send_raw(&wake_backfill_close())
            .await
    }

    async fn next_frame(&mut self, timeout_secs: u64) -> Result<Option<FeedFrame>, Self::Error> {
        match self
            .connection_mut()?
            .next_event(Duration::from_secs(timeout_secs))
            .await
        {
            Ok(message) => Ok(Some(feed_frame(message)?)),
            // A quiet relay is not a broken one. The loop turns this into a
            // coverage checkpoint; anything else it turns into a reconnect.
            Err(WsClientError::Timeout) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// Extract a channel UUID from an event's `d` tag.
///
/// kind:39002 (NIP-29 group members) is a parameterized-replaceable event
/// whose `d` tag is the channel id — not `h`, which is what channel-scoped
/// *content* events (and membership-change notifications) carry instead. Only
/// [`RelayFeed::discover_channels`]'s one-shot query reads this; nothing else
/// in the crate needs it.
fn extract_d_tag_uuid(event: &Value) -> Option<Uuid> {
    event.get("tags")?.as_array()?.iter().find_map(|tag| {
        let parts = tag.as_array()?;
        if parts.first()?.as_str()? != "d" {
            return None;
        }
        parts.get(1)?.as_str()?.parse::<Uuid>().ok()
    })
}

/// Translate a relay message into the frame the feed understands.
///
/// Everything the feed does not read collapses to [`FeedFrame::Other`] rather
/// than being enumerated here — the feed's ignore list is stated once, in
/// [`crate::feed::classify`], and duplicating it would let the two drift.
///
/// An EVENT whose id or signature does not check out becomes
/// [`FeedFrame::Rejected`], not [`FeedFrame::Other`]: a relay serving forged
/// events is an incident, and collapsing it into the same bucket as a NOTICE
/// would make it invisible. The connection is deliberately *not* torn down —
/// a relay that can forge one event can forge a stream of them, and turning
/// each into a reconnect would hand it a way to keep the feed permanently off
/// the air.
///
/// Verification is Schnorr and therefore CPU-bound. It runs inline rather than
/// on a blocking pool, matching the other client-side consumer of relay events
/// (`buzz-acp/src/lib.rs`); the relay's own ingest path uses
/// `spawn_blocking` because it verifies every write in the community, whereas
/// this feed sees one agent's mentions. Keeping it inline is what lets this
/// crate stay free of an async runtime dependency.
fn feed_frame(message: RelayMessage) -> Result<FeedFrame, WsClientError> {
    Ok(match message {
        RelayMessage::Event {
            subscription_id,
            event,
        } => match buzz_core::verify_event(&event) {
            Ok(()) => FeedFrame::Event {
                subscription_id,
                event: serde_json::to_value(*event)?,
            },
            Err(error) => FeedFrame::Rejected {
                subscription_id,
                event_id: event.id.to_hex(),
                reason: error.to_string(),
            },
        },
        RelayMessage::Eose { subscription_id } => FeedFrame::Eose { subscription_id },
        RelayMessage::Closed {
            subscription_id,
            message,
        } => FeedFrame::Closed {
            subscription_id,
            message,
        },
        RelayMessage::Ok(_)
        | RelayMessage::Notice { .. }
        | RelayMessage::Auth { .. }
        | RelayMessage::Count { .. } => FeedFrame::Other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_ws_client::OkResponse;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    /// A real, local, in-process WebSocket relay double: completes the NIP-42
    /// handshake for real, then forwards every subsequent client frame
    /// (parsed as a raw JSON array) to the returned receiver.
    ///
    /// Exists because a synthetic [`FeedFrame`]/[`RelayMessage`] test cannot
    /// prove anything about what [`RelayFeed`] actually puts on the wire — the
    /// same blind spot that hid the original per-channel fan-out bug this
    /// module's design responds to (see `crate::feed`'s module docs). Driving
    /// the real `NostrWsConnection` handshake and `send_raw` path against a
    /// real (if minimal) server closes that gap without needing a full relay,
    /// Postgres, or Redis — nothing here inspects delivery, only what
    /// `RelayFeed`'s own methods choose to send.
    async fn recording_relay_server() -> (String, tokio::sync::mpsc::UnboundedReceiver<Value>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };

            // NIP-42: challenge, then the client's signed AUTH event, then OK.
            // No `class` tag is ever requested by `RelayFeed` (see its `new`
            // doc), so a bare `OK true ""` is the correct reply — nothing here
            // needs to confirm a restricted class.
            let challenge = json!(["AUTH", "mock-relay-challenge"]).to_string();
            if ws.send(WsMessage::Text(challenge.into())).await.is_err() {
                return;
            }
            let auth_event_id = loop {
                let Some(Ok(WsMessage::Text(text))) = ws.next().await else {
                    return;
                };
                let Ok(arr) = serde_json::from_str::<Vec<Value>>(&text) else {
                    continue;
                };
                if arr.first().and_then(Value::as_str) == Some("AUTH") {
                    let Some(id) = arr.get(1).and_then(|e| e.get("id")).and_then(Value::as_str)
                    else {
                        return;
                    };
                    break id.to_string();
                }
            };
            let ok = json!(["OK", auth_event_id, true, ""]).to_string();
            if ws.send(WsMessage::Text(ok.into())).await.is_err() {
                return;
            }

            // Post-handshake: record every frame, replying to nothing. The
            // tests here only assert on what was sent, not on delivery.
            while let Some(Ok(WsMessage::Text(text))) = ws.next().await {
                if let Ok(arr) = serde_json::from_str::<Vec<Value>>(&text) {
                    if tx.send(Value::Array(arr)).is_err() {
                        return;
                    }
                }
            }
        });

        (format!("ws://{addr}"), rx)
    }

    /// Pull the next client frame the mock relay observed, bounded so a bug
    /// that stops sending fails the test instead of hanging the suite.
    async fn next_sent_frame(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Value>) -> Value {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the mock relay must observe a frame within the test timeout")
            .expect("the recorder channel must not close mid-test")
    }

    /// Alex's review on `2aa51a6e6`/`9ac4d6a09`: the unit-level filter
    /// regression proves a floor-bound filter is correct in isolation, but
    /// proves nothing about whether `RelayFeed`'s three separate subscribe
    /// methods actually receive and forward the *same* floor when driven for
    /// real. This drives the genuine `RelayFeed` — real NIP-42 handshake,
    /// real `send_raw` calls — against [`recording_relay_server`] and reads
    /// back exactly what went out over the wire.
    #[tokio::test]
    async fn membership_live_and_backfill_requests_all_carry_the_same_since() {
        let (url, mut sent) = recording_relay_server().await;
        let keys = Keys::generate();
        let mut feed = RelayFeed::new(url, keys, None);
        feed.connect()
            .await
            .expect("connects and authenticates against the mock relay");

        let since = 1_700_000_000_u64;
        let channel_a = Uuid::from_u128(0xAAAA);
        let channel_b = Uuid::from_u128(0xBBBB);

        feed.subscribe_membership(since)
            .await
            .expect("subscribes membership");
        feed.subscribe_channel_live(channel_a, since)
            .await
            .expect("subscribes channel a live");
        feed.subscribe_channel_live(channel_b, since)
            .await
            .expect("subscribes channel b live");
        feed.subscribe_backfill(since, None)
            .await
            .expect("subscribes backfill");

        let mut observed = Vec::new();
        for _ in 0..4 {
            let frame = next_sent_frame(&mut sent).await;
            assert_eq!(
                frame[0], "REQ",
                "every call here issues a REQ, got {frame:?}"
            );
            let observed_since = frame[2]["since"]
                .as_u64()
                .unwrap_or_else(|| panic!("filter must carry `since`: {frame:?}"));
            observed.push((
                frame[1].as_str().unwrap_or_default().to_string(),
                observed_since,
            ));
        }

        assert!(
            observed.iter().all(|(_, s)| *s == since),
            "membership, every per-channel live subscription, and backfill must \
             all carry the identical replay floor — got {observed:?}"
        );
    }

    #[test]
    fn an_event_frame_keeps_its_subscription_and_json() {
        let keys = Keys::generate();
        let event = nostr::EventBuilder::text_note("hello")
            .sign_with_keys(&keys)
            .expect("signs");
        let id = event.id.to_hex();

        let frame = feed_frame(RelayMessage::Event {
            subscription_id: "sub".to_string(),
            event: Box::new(event),
        })
        .expect("an event converts");

        match frame {
            FeedFrame::Event {
                subscription_id,
                event,
            } => {
                assert_eq!(subscription_id, "sub");
                assert_eq!(event["id"], id, "the id must survive the round trip");
            }
            other => panic!("expected an event frame, got {other:?}"),
        }
    }

    /// Rebuild an event from JSON after `mutate` has changed it, the way a
    /// hostile relay would put it on the wire: `nostr::Event` deserializes by
    /// shape, so the tampered fields survive parsing and only the signature
    /// disagrees.
    fn tampered_event(mutate: impl FnOnce(&mut serde_json::Value)) -> nostr::Event {
        use nostr::JsonUtil;
        let keys = Keys::generate();
        let event = nostr::EventBuilder::text_note("hello")
            .sign_with_keys(&keys)
            .expect("signs");
        let mut json: serde_json::Value =
            serde_json::from_str(&event.as_json()).expect("an event round-trips through JSON");
        mutate(&mut json);
        nostr::Event::from_json(json.to_string()).expect("a tampered event still parses")
    }

    #[test]
    fn an_event_the_relay_could_have_forged_never_becomes_a_trigger() {
        // The finding this pins: `buzz-ws-client` deserializes an EVENT frame
        // by shape and never verifies it, and the projection into `FeedFrame`
        // throws the signature away. Without a check here, a relay can claim
        // any author it likes — including the owner, who is exactly the author
        // an agent's respond-to policy is most permissive about — and spend a
        // real deploy on an event nobody wrote.
        //
        // Both halves are covered: a swapped `pubkey` breaks the id (the id is
        // a hash over the author), and a swapped `sig` breaks only the
        // signature. Checking the id alone would pass the second.
        let impersonator = Keys::generate();
        let cases = [
            (
                "a forged author",
                tampered_event(|json| {
                    json["pubkey"] = serde_json::Value::String(impersonator.public_key().to_hex());
                }),
            ),
            (
                "a forged signature",
                tampered_event(|json| {
                    json["sig"] = serde_json::Value::String("0".repeat(128));
                }),
            ),
        ];

        for (what, event) in cases {
            let frame = feed_frame(RelayMessage::Event {
                subscription_id: "sub".to_string(),
                event: Box::new(event),
            })
            .expect("converts");

            match frame {
                FeedFrame::Rejected { reason, .. } => {
                    assert!(!reason.is_empty(), "{what} must say why it was refused");
                }
                other => panic!("{what} must not be projected, got {other:?}"),
            }
        }
    }

    #[test]
    fn frames_the_feed_ignores_collapse_to_other() {
        // Named individually so a new relay message kind fails to compile here
        // rather than silently becoming something the feed acts on.
        for message in [
            RelayMessage::Ok(OkResponse {
                event_id: "ff".repeat(32),
                accepted: true,
                message: String::new(),
            }),
            RelayMessage::Notice {
                message: "slow down".to_string(),
            },
            RelayMessage::Auth {
                challenge: "c".to_string(),
            },
            RelayMessage::Count {
                subscription_id: "sub".to_string(),
                count: 3,
            },
        ] {
            assert!(matches!(
                feed_frame(message).expect("converts"),
                FeedFrame::Other
            ));
        }
    }
}
