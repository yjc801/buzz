//! Owner-signed **start requests**: "start this agent now", with no mention.
//!
//! The mention feed ([`crate::feed`]) is this daemon's other trigger, and it
//! exists for someone *talking to* an agent. An owner pressing Start on an
//! agent with Remote wake on is asking for the same deploy with no message to
//! carry it. Before this, that press deployed from the owner's own machine
//! with whatever substrate credential it held — for a member of a hosted
//! community, none. A start request routes it here instead, so the deploy
//! spends the operator's credential exactly as a mention's does.
//!
//! # Wire
//!
//! A [`KIND_GIFT_WRAP`] `#p`-tagged to the agent and signed by the pair's
//! **envelope key** ([`start_request_envelope_keys`]; see
//! [`KIND_WAKER_START_REQUEST`] for why not the owner's own), its content
//! NIP-44 encrypted envelope key→agent, its plaintext a
//! [`SignedStartRequest`]. The daemon reads it on the bundle tap's connection
//! under its own subscription, [`START_REQUEST_SUBSCRIPTION_ID`], with
//! `authors` pinned to that envelope key and `since` bounded to the freshness
//! window — see [`start_request_req`].
//!
//! # Trust
//!
//! Only the inner signature counts. The envelope key is what keeps the relay
//! query to the owner's own traffic — only the owner and the agent can derive
//! it, and the relay refuses an event whose signature does not match its
//! `pubkey` — but it grants nothing. [`SignedStartRequest::verify`] checks, in order: the pinned
//! owner, the BIP-340 signature, the body, the target agent, and freshness.
//! An accepted request becomes an ordinary [`TriggerEvent`] authored by the
//! owner and dated `requested_at`, and takes the mention path from there: the
//! cursor's durable claim (which is also what refuses a replayed envelope, by
//! event id), the in-flight collapse and debounce, the idempotent deploy with
//! `requested_at` as the replay floor, and one re-drive.

use buzz_core::kind::{KIND_GIFT_WRAP, KIND_WAKER_START_REQUEST};
use buzz_ws_client::RelayMessage;
use nostr::hashes::sha256::Hash as Sha256Hash;
use nostr::hashes::Hash as _;
use nostr::secp256k1::schnorr::Signature;
use nostr::secp256k1::{Keypair, Message, XOnlyPublicKey};
use nostr::{Event, Keys, PublicKey, SecretKey, SECP256K1};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::bundle_feed::NIP44_CONTENT_LEN_RANGE;
use crate::decide::{normalize_pubkey, TriggerEvent};

/// Domain separator mixed into every start-request digest. Distinct from
/// [`crate::bundle::BUNDLE_DOMAIN`] and the enrolment domains, so no other
/// owner signature can be replayed as a start request.
pub const START_REQUEST_DOMAIN: &[u8] = b"buzz-waker:start-request:v1\0";

/// Domain separator for the envelope key derived from the owner↔agent shared
/// secret ([`start_request_envelope_keys`]).
pub const START_REQUEST_ENVELOPE_DOMAIN: &[u8] = b"buzz-waker:start-request-envelope:v1\0";

/// Subscription id for the start-request half of the bundle tap's connection.
pub const START_REQUEST_SUBSCRIPTION_ID: &str = "buzz-waker-start";

/// How old a request may be and still start the agent.
///
/// The same bound a mention is held to ([`crate::WAKE_DELIVERABLE_AGE_SECS`]):
/// past it, the harness a wake starts could not replay back to the request
/// anyway, and the owner's desktop has long since stopped waiting for it.
pub const START_REQUEST_MAX_AGE_SECS: u64 = crate::WAKE_DELIVERABLE_AGE_SECS;

/// How far in the future a request may be dated — clock skew between the
/// owner's machine and this daemon, not a scheduling feature.
pub const START_REQUEST_MAX_SKEW_SECS: u64 = 60;

/// Bound on how many envelopes one (re)subscribe can replay. The query is
/// pinned to the pair's envelope key, so everything it can return is the
/// owner's own — one per Start press, over a window of minutes. The limit
/// only bounds the owner; nobody else can place an envelope in this result
/// set and push a real request out of it.
const START_REQUEST_QUERY_LIMIT: u32 = 64;

/// What can go wrong turning a delivered envelope into a trusted request.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StartRequestError {
    /// The request names an owner other than the one pinned at enrolment.
    /// Checked before the signature, as [`crate::bundle`] does.
    #[error("start request owner {found} is not the enrolment-pinned owner {pinned}")]
    WrongOwner {
        /// The owner the request claims.
        found: String,
        /// The owner pinned at enrolment.
        pinned: String,
    },
    /// The owner pubkey is not a valid x-only key.
    #[error("malformed owner public key: {0}")]
    MalformedOwnerKey(String),
    /// The signature is not 64 bytes of hex.
    #[error("malformed signature: {0}")]
    MalformedSignature(String),
    /// BIP-340 verification failed over the received body bytes.
    #[error("start request signature verification failed")]
    BadSignature,
    /// The envelope was not signed by this owner↔agent pair's envelope key.
    /// The relay query pins that key, so this is a relay that ignored the
    /// filter; refused rather than trusted to the inner signature alone.
    #[error("start request envelope signed by {found}, not the pair's envelope key")]
    WrongEnvelopeSigner {
        /// The key that signed the envelope.
        found: String,
    },
    /// The envelope or the signed body is not the expected shape.
    #[error("malformed start request: {0}")]
    Malformed(String),
    /// A valid request for a different agent — a relay misroute or a replay
    /// across this owner's agents.
    #[error("start request is for agent {found}, not this one")]
    WrongAgent {
        /// The agent the request names.
        found: String,
    },
    /// Older than [`START_REQUEST_MAX_AGE_SECS`].
    #[error("start request from {requested_at} is too old to act on (now {now})")]
    Stale {
        /// When the owner made the request, unix seconds.
        requested_at: u64,
        /// The time this daemon checked it, unix seconds.
        now: u64,
    },
    /// Dated further ahead than [`START_REQUEST_MAX_SKEW_SECS`].
    #[error("start request is dated {requested_at}, in the future (now {now})")]
    FromTheFuture {
        /// When the request claims it was made, unix seconds.
        requested_at: u64,
        /// The time this daemon checked it, unix seconds.
        now: u64,
    },
}

/// The signed statement: this owner asks for this agent to be started now.
///
/// Deliberately carries no deploy payload. What gets deployed is the agent's
/// current launch bundle, admitted on its own path; a start request can only
/// ask for that, never change it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRequestBody {
    /// Hex pubkey of the agent to start.
    pub agent_pubkey: String,
    /// When the owner asked, unix seconds. Becomes the wake's replay floor.
    pub requested_at: u64,
}

/// A [`StartRequestBody`] as signed JSON bytes plus the owner's signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedStartRequest {
    /// The exact bytes that were signed, as a UTF-8 JSON string.
    pub body_json: String,
    /// Hex x-only pubkey of the signing owner.
    pub owner_pubkey: String,
    /// Hex BIP-340 signature over [`start_request_digest`] of `body_json`.
    pub sig: String,
}

/// The digest a start-request signature covers:
/// `SHA-256(START_REQUEST_DOMAIN || body_json)`.
#[must_use]
pub fn start_request_digest(body_json: &str) -> [u8; 32] {
    let mut preimage = Vec::with_capacity(START_REQUEST_DOMAIN.len() + body_json.len());
    preimage.extend_from_slice(START_REQUEST_DOMAIN);
    preimage.extend_from_slice(body_json.as_bytes());
    Sha256Hash::hash(&preimage).to_byte_array()
}

impl SignedStartRequest {
    /// Sign a body with the owner's keypair.
    ///
    /// # Errors
    /// [`StartRequestError::Malformed`] if the body cannot be serialized.
    pub fn sign(body: &StartRequestBody, owner: &Keypair) -> Result<Self, StartRequestError> {
        let body_json = serde_json::to_string(body)
            .map_err(|e| StartRequestError::Malformed(format!("could not serialize: {e}")))?;
        let message = Message::from_digest(start_request_digest(&body_json));
        let sig = SECP256K1.sign_schnorr(&message, owner);
        let (xonly, _) = owner.x_only_public_key();
        Ok(Self {
            body_json,
            owner_pubkey: hex::encode(xonly.serialize()),
            sig: hex::encode(sig.serialize()),
        })
    }

    /// Verify against the enrolment-pinned owner and this agent, at `now`.
    ///
    /// Same order as [`crate::bundle::SignedLaunchBundle::verify`]: identity
    /// before cryptography, and nothing parsed before the signature holds.
    ///
    /// # Errors
    /// See [`StartRequestError`] — every variant is a refusal to start.
    pub fn verify(
        &self,
        pinned_owner_pubkey: &str,
        agent_pubkey: &str,
        now: u64,
    ) -> Result<StartRequestBody, StartRequestError> {
        if normalize_pubkey(&self.owner_pubkey) != normalize_pubkey(pinned_owner_pubkey) {
            return Err(StartRequestError::WrongOwner {
                found: self.owner_pubkey.clone(),
                pinned: pinned_owner_pubkey.to_string(),
            });
        }

        let key_bytes = hex::decode(&self.owner_pubkey)
            .map_err(|e| StartRequestError::MalformedOwnerKey(e.to_string()))?;
        let xonly = XOnlyPublicKey::from_slice(&key_bytes)
            .map_err(|e| StartRequestError::MalformedOwnerKey(e.to_string()))?;
        let sig_bytes = hex::decode(&self.sig)
            .map_err(|e| StartRequestError::MalformedSignature(e.to_string()))?;
        let sig = Signature::from_slice(&sig_bytes)
            .map_err(|e| StartRequestError::MalformedSignature(e.to_string()))?;
        let message = Message::from_digest(start_request_digest(&self.body_json));
        if SECP256K1.verify_schnorr(&sig, &message, &xonly).is_err() {
            return Err(StartRequestError::BadSignature);
        }

        let body: StartRequestBody = serde_json::from_str(&self.body_json)
            .map_err(|e| StartRequestError::Malformed(e.to_string()))?;
        if normalize_pubkey(&body.agent_pubkey) != normalize_pubkey(agent_pubkey) {
            return Err(StartRequestError::WrongAgent {
                found: body.agent_pubkey,
            });
        }
        if body.requested_at > now.saturating_add(START_REQUEST_MAX_SKEW_SECS) {
            return Err(StartRequestError::FromTheFuture {
                requested_at: body.requested_at,
                now,
            });
        }
        if now.saturating_sub(body.requested_at) > START_REQUEST_MAX_AGE_SECS {
            return Err(StartRequestError::Stale {
                requested_at: body.requested_at,
                now,
            });
        }
        Ok(body)
    }
}

/// The keys that sign start-request envelopes for one owner↔agent pair.
///
/// `SHA-256(START_REQUEST_ENVELOPE_DOMAIN || x(ECDH))`, where the ECDH is the
/// same x-only shared point NIP-44 uses. It is symmetric, so the owner derives
/// it from `(owner secret, agent pubkey)` and this daemon from `(agent secret,
/// owner pubkey)`, and nobody without one of those two secrets can sign as it.
/// That is what lets [`start_request_req`] pin `authors`: the relay verifies
/// every event's signature at ingest, so a stranger's gift wraps tagged to the
/// agent cannot enter the result set and crowd a real request out of its
/// `limit`. It is a separate key rather than the owner's own so that start
/// requests stay out of the bundle tap's owner-pinned window (see
/// [`KIND_WAKER_START_REQUEST`]).
///
/// # Errors
/// [`StartRequestError::Malformed`] if `peer` is not a valid key, or (with
/// negligible probability) the digest is not a valid secret key.
pub fn start_request_envelope_keys(
    own_secret: &SecretKey,
    peer: &PublicKey,
) -> Result<Keys, StartRequestError> {
    let shared = nostr::util::generate_shared_key(own_secret, peer)
        .map_err(|e| StartRequestError::Malformed(format!("envelope key agreement: {e}")))?;
    let mut preimage = Vec::with_capacity(START_REQUEST_ENVELOPE_DOMAIN.len() + shared.len());
    preimage.extend_from_slice(START_REQUEST_ENVELOPE_DOMAIN);
    preimage.extend_from_slice(&shared);
    let secret = SecretKey::from_slice(&Sha256Hash::hash(&preimage).to_byte_array())
        .map_err(|e| StartRequestError::Malformed(format!("envelope key derivation: {e}")))?;
    Ok(Keys::new(secret))
}

/// The REQ for one agent's start requests, sent on the bundle tap's
/// connection at every (re)connect.
///
/// `authors` is the pair's envelope key ([`start_request_envelope_keys`]), so
/// only the owner's envelopes count against `limit`. `since` keeps a
/// reconnect from replaying anything [`SignedStartRequest::verify`] would
/// refuse as stale anyway; the cursor refuses the in-window rest as
/// duplicates.
pub(crate) fn start_request_req(
    agent_pubkey: &str,
    envelope_author: &PublicKey,
    now: u64,
) -> serde_json::Value {
    json!([
        "REQ",
        START_REQUEST_SUBSCRIPTION_ID,
        {
            "kinds": [KIND_GIFT_WRAP],
            "authors": [envelope_author.to_hex()],
            "#p": [agent_pubkey],
            "since": now.saturating_sub(START_REQUEST_MAX_AGE_SECS),
            "limit": START_REQUEST_QUERY_LIMIT,
        }
    ])
}

/// What one relay message means for the start-request subscription.
#[derive(Debug)]
pub(crate) enum StartRequestFrame<'a> {
    /// An event on this subscription, not yet verified or decrypted.
    Event(&'a Event),
    /// The relay closed this subscription.
    Closed(&'a str),
    /// Not this subscription's message.
    NotOurs,
}

/// Classify one relay message for the start-request subscription.
pub(crate) fn start_request_frame(message: &RelayMessage) -> StartRequestFrame<'_> {
    match message {
        RelayMessage::Event {
            subscription_id,
            event,
        } if subscription_id == START_REQUEST_SUBSCRIPTION_ID => StartRequestFrame::Event(event),
        RelayMessage::Closed {
            subscription_id,
            message,
        } if subscription_id == START_REQUEST_SUBSCRIPTION_ID => StartRequestFrame::Closed(message),
        _ => StartRequestFrame::NotOurs,
    }
}

/// Turn one delivered envelope into a wake trigger, or say why not.
///
/// `envelope_author` is [`start_request_envelope_keys`]' public key for this
/// agent and its owner. `Ok(None)` is an event that is not a gift wrap at
/// all.
///
/// # Errors
/// A [`StartRequestError`] for anything that looked like a request and was
/// refused. The caller logs and continues: junk tagged to an agent must not
/// be able to take the tap offline.
pub(crate) fn admit_start_request(
    keys: &Keys,
    owner_pubkey: &str,
    envelope_author: &PublicKey,
    event: &Event,
    now: u64,
) -> Result<Option<TriggerEvent>, StartRequestError> {
    if buzz_core::kind::event_kind_u32(event) != KIND_GIFT_WRAP {
        return Ok(None);
    }
    if event.pubkey != *envelope_author {
        return Err(StartRequestError::WrongEnvelopeSigner {
            found: event.pubkey.to_hex(),
        });
    }
    buzz_core::verify_event(event).map_err(|e| StartRequestError::Malformed(e.to_string()))?;
    if !NIP44_CONTENT_LEN_RANGE.contains(&event.content.len()) {
        return Err(StartRequestError::Malformed(format!(
            "content length {} outside the NIP-44 range",
            event.content.len()
        )));
    }
    let plaintext = nostr::nips::nip44::decrypt(keys.secret_key(), &event.pubkey, &event.content)
        .map_err(|e| StartRequestError::Malformed(format!("could not decrypt: {e}")))?;
    let signed: SignedStartRequest = serde_json::from_str(&plaintext)
        .map_err(|e| StartRequestError::Malformed(e.to_string()))?;
    let agent_pubkey = keys.public_key().to_hex();
    let body = signed.verify(owner_pubkey, &agent_pubkey, now)?;
    Ok(Some(TriggerEvent {
        id: event.id.to_hex(),
        author: normalize_pubkey(owner_pubkey),
        kind: KIND_WAKER_START_REQUEST,
        p_tags: vec![agent_pubkey],
        created_at: body.requested_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::nips::nip44::{self, Version};
    use nostr::{EventBuilder, Kind, Tag, Timestamp};

    const NOW: u64 = 1_800_000_000;

    fn signed_for(owner: &Keys, agent: &Keys, requested_at: u64) -> SignedStartRequest {
        let body = StartRequestBody {
            agent_pubkey: agent.public_key().to_hex(),
            requested_at,
        };
        SignedStartRequest::sign(&body, &owner.secret_key().keypair(SECP256K1)).unwrap()
    }

    /// The pair's envelope key, derived from the owner's side as the desktop
    /// does.
    fn envelope_keys(owner: &Keys, agent: &Keys) -> Keys {
        start_request_envelope_keys(owner.secret_key(), &agent.public_key()).unwrap()
    }

    /// Admit from the daemon's side, with the envelope key it derives.
    fn admit(
        agent: &Keys,
        owner: &Keys,
        event: &Event,
    ) -> Result<Option<TriggerEvent>, StartRequestError> {
        let author = start_request_envelope_keys(agent.secret_key(), &owner.public_key())
            .unwrap()
            .public_key();
        admit_start_request(agent, &owner.public_key().to_hex(), &author, event, NOW)
    }

    /// A start-request envelope: a gift wrap signed by `signer`, NIP-44 from
    /// `signer` to the agent. The desktop's `signer` is [`envelope_keys`].
    fn envelope(signer: &Keys, agent: &Keys, signed: &SignedStartRequest) -> Event {
        let plaintext = serde_json::to_string(signed).unwrap();
        let content = nip44::encrypt(
            signer.secret_key(),
            &agent.public_key(),
            plaintext,
            Version::V2,
        )
        .unwrap();
        EventBuilder::new(Kind::Custom(KIND_GIFT_WRAP as u16), content)
            .tags([Tag::public_key(agent.public_key())])
            .custom_created_at(Timestamp::from(NOW))
            .sign_with_keys(signer)
            .unwrap()
    }

    #[test]
    fn a_fresh_owner_request_becomes_a_trigger_dated_when_the_owner_asked() {
        let (owner, agent) = (Keys::generate(), Keys::generate());
        let event = envelope(
            &envelope_keys(&owner, &agent),
            &agent,
            &signed_for(&owner, &agent, NOW - 30),
        );

        let trigger = admit(&agent, &owner, &event)
            .unwrap()
            .expect("a start request");
        assert_eq!(trigger.id, event.id.to_hex());
        assert_eq!(trigger.author, owner.public_key().to_hex());
        assert_eq!(
            trigger.created_at,
            NOW - 30,
            "the replay floor is the request time"
        );
        assert_eq!(trigger.kind, KIND_WAKER_START_REQUEST);
    }

    #[test]
    fn a_request_signed_by_anyone_but_the_pinned_owner_is_refused() {
        let (owner, stranger, agent) = (Keys::generate(), Keys::generate(), Keys::generate());
        let event = envelope(
            &envelope_keys(&owner, &agent),
            &agent,
            &signed_for(&stranger, &agent, NOW),
        );
        assert!(matches!(
            admit(&agent, &owner, &event),
            Err(StartRequestError::WrongOwner { .. })
        ));
    }

    #[test]
    fn a_tampered_body_fails_the_signature() {
        let (owner, agent) = (Keys::generate(), Keys::generate());
        let mut signed = signed_for(&owner, &agent, NOW);
        signed.body_json = signed
            .body_json
            .replace(&NOW.to_string(), &(NOW + 1).to_string());
        assert_eq!(
            signed.verify(
                &owner.public_key().to_hex(),
                &agent.public_key().to_hex(),
                NOW
            ),
            Err(StartRequestError::BadSignature)
        );
    }

    #[test]
    fn a_request_for_another_agent_is_refused() {
        let (owner, agent, other) = (Keys::generate(), Keys::generate(), Keys::generate());
        // Encrypted to `agent`, but the owner signed it for `other`.
        let event = envelope(
            &envelope_keys(&owner, &agent),
            &agent,
            &signed_for(&owner, &other, NOW),
        );
        assert!(matches!(
            admit(&agent, &owner, &event),
            Err(StartRequestError::WrongAgent { .. })
        ));
    }

    #[test]
    fn freshness_is_bounded_on_both_sides() {
        let (owner, agent) = (Keys::generate(), Keys::generate());
        let (owner_hex, agent_hex) = (owner.public_key().to_hex(), agent.public_key().to_hex());

        let oldest_ok = signed_for(&owner, &agent, NOW - START_REQUEST_MAX_AGE_SECS);
        assert!(oldest_ok.verify(&owner_hex, &agent_hex, NOW).is_ok());
        let stale = signed_for(&owner, &agent, NOW - START_REQUEST_MAX_AGE_SECS - 1);
        assert!(matches!(
            stale.verify(&owner_hex, &agent_hex, NOW),
            Err(StartRequestError::Stale { .. })
        ));

        let skewed_ok = signed_for(&owner, &agent, NOW + START_REQUEST_MAX_SKEW_SECS);
        assert!(skewed_ok.verify(&owner_hex, &agent_hex, NOW).is_ok());
        let future = signed_for(&owner, &agent, NOW + START_REQUEST_MAX_SKEW_SECS + 1);
        assert!(matches!(
            future.verify(&owner_hex, &agent_hex, NOW),
            Err(StartRequestError::FromTheFuture { .. })
        ));
    }

    #[test]
    fn owner_and_daemon_derive_the_same_envelope_key_and_nobody_else_does() {
        let (owner, agent) = (Keys::generate(), Keys::generate());
        let from_owner = envelope_keys(&owner, &agent);
        let from_daemon =
            start_request_envelope_keys(agent.secret_key(), &owner.public_key()).unwrap();
        assert_eq!(from_owner.public_key(), from_daemon.public_key());
        assert_ne!(from_owner.public_key(), owner.public_key());

        // Another owner of the same agent (or another agent of this owner)
        // gets a different key, so the pin is per pair.
        let other_owner = envelope_keys(&Keys::generate(), &agent);
        assert_ne!(other_owner.public_key(), from_owner.public_key());
    }

    #[test]
    fn an_envelope_not_signed_by_the_pairs_key_is_refused_even_if_the_body_is_genuine() {
        // The owner's own key (a launch bundle's signer) and a stranger's
        // both fail: the query pins the envelope key, and admission holds the
        // same line rather than trusting a relay to have applied the filter.
        let (owner, agent) = (Keys::generate(), Keys::generate());
        let signed = signed_for(&owner, &agent, NOW);
        for signer in [owner.clone(), Keys::generate()] {
            assert!(matches!(
                admit(&agent, &owner, &envelope(&signer, &agent, &signed)),
                Err(StartRequestError::WrongEnvelopeSigner { .. })
            ));
        }
    }

    #[test]
    fn the_req_is_time_bounded_and_pinned_to_the_pairs_envelope_key() {
        let (owner, agent) = (Keys::generate(), Keys::generate());
        let author = envelope_keys(&owner, &agent).public_key();
        let agent_hex = agent.public_key().to_hex();
        let req = start_request_req(&agent_hex, &author, NOW);
        let filter = &req[2];
        assert_eq!(req[1], START_REQUEST_SUBSCRIPTION_ID);
        assert_eq!(filter["kinds"], json!([KIND_GIFT_WRAP]));
        assert_eq!(filter["authors"], json!([author.to_hex()]));
        assert_eq!(filter["#p"], json!([agent_hex]));
        assert_eq!(filter["since"], json!(NOW - START_REQUEST_MAX_AGE_SECS));
    }
}
