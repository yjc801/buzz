//! The relay-backed [`FeedTransport`] — the only part of the feed that owns a
//! socket.
//!
//! Deliberately thin. Everything with a rule in it lives in [`crate::feed`] and
//! is tested there; what remains here is connect, send, receive, and the
//! translation from the ws client's message set into [`FeedFrame`]. If this
//! file starts making decisions, they belong upstairs.

use std::time::Duration;

use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};
use nostr::{Keys, Tag};
use serde_json::Value;

use crate::feed::{FeedFrame, FeedTransport};

/// A NIP-42-authenticated connection to one relay, reconnectable in place.
///
/// Holds the credentials rather than a live socket, because a reconnect has to
/// rebuild the socket while the caller's cursor and in-flight set survive.
pub struct RelayFeed {
    relay_url: String,
    keys: Keys,
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
        Self {
            relay_url: relay_url.into(),
            keys,
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

    async fn send(&mut self, frame: &Value) -> Result<(), Self::Error> {
        self.connection_mut()?.send_raw(frame).await
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
fn feed_frame(message: RelayMessage) -> Result<FeedFrame, WsClientError> {
    Ok(match message {
        RelayMessage::Event {
            subscription_id,
            event,
        } => FeedFrame::Event {
            subscription_id,
            event: serde_json::to_value(*event)?,
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
