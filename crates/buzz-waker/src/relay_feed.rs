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

use crate::feed::{
    wake_backfill_close, wake_backfill_req, wake_live_req, FeedFrame, FeedTransport,
};

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

    async fn subscribe_live(&mut self, since: u64) -> Result<(), Self::Error> {
        // `self.agent_pubkey`, never an argument: the filter's `#p` and the
        // socket's authenticated identity are the same value by construction.
        let req = wake_live_req(&self.agent_pubkey, since);
        self.connection_mut()?.send_raw(&req).await
    }

    async fn subscribe_backfill(&mut self, since: u64, until: u64) -> Result<(), Self::Error> {
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
