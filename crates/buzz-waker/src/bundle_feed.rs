//! The bundle-delivery tap — daemon-side half of bundle transport
//! (`PLANS/BUZZ_WAKER_DESIGN.md` §11).
//!
//! One connection per watched agent, held alongside
//! [`crate::relay_feed::RelayFeed`]'s mention feed and
//! [`crate::presence_feed::run_presence_tap`]'s presence tap — a third
//! concern, a third connection, matching this daemon's existing "one
//! connection per concern" shape rather than retrofitting either of the
//! other two. [`crate::feed::FeedTransport`] is purpose-built for the
//! mention feed's channel-discovery → membership → live → backfill
//! lifecycle and has no generic `subscribe(filter)`; this tap's filter
//! shape (global, `authors` + `#p` pinned, no channel scoping) doesn't fit
//! it and doesn't need its cursor/replay machinery either — see the design
//! doc's own reasoning for why bundle delivery only ever cares about the
//! **latest valid version**, never a missed intermediate one.
//!
//! # What this tap does *not* do
//!
//! Resolve anything. It decrypts, verifies the inner signature against the
//! enrolment-pinned owner ([`crate::floors::FloorStore`], **G2**), and
//! admits the version — the same floor/verify split
//! [`crate::bundle::SignedLaunchBundle::verify`] and
//! [`crate::floors::FloorStore::admit`] already implement and already test
//! independently. This module is the wiring between them and a live socket,
//! nothing more.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use buzz_core::kind::KIND_WAKER_LAUNCH_BUNDLE;
use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};
use nostr::{Keys, Tag};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroize;

use crate::bundle::{LaunchBundleBody, SignedLaunchBundle};
use crate::decide::normalize_pubkey;
use crate::feed::reconnect_delay_ms;
use crate::floors::FloorStore;

/// Subscription id for one agent's bundle-delivery tap. Fixed, like the
/// mention feed's and the presence tap's own ids — a reconnect replaces the
/// old subscription rather than piling up a fresh one.
pub const BUNDLE_TAP_SUBSCRIPTION_ID: &str = "buzz-waker-bundle";

/// How long to wait for a frame before treating the tap connection as idle.
///
/// Wider than the presence tap's own timeout: a bundle is reissued on
/// enrolment and on config change only (**G3** — never as a liveness ping),
/// so long quiet stretches are the normal case, not a symptom.
pub const BUNDLE_TAP_IDLE_TIMEOUT_SECS: u64 = 300;

/// NIP-44 v2 payloads are base64, 132–87472 characters
/// (`buzz_core::pairing::session`'s own NIP-AB validation applies the same
/// range) — reject anything outside it before attempting decryption rather
/// than handing an oversized or malformed string to the decryptor.
const NIP44_CONTENT_LEN_RANGE: std::ops::RangeInclusive<usize> = 132..=87472;

/// The REQ filter for one agent's bundle tap: global, `authors` pinned to
/// the enrolment-pinned owner, `#p` pinned to the target agent.
///
/// `authors` is what actually stops a flooding attacker from ever appearing
/// in this query's results — ordinary ingest already refuses a forged
/// `event.pubkey`, so `authors` here is redundant with what the relay
/// enforces at write time, kept as defense in depth and to make the query's
/// own intent explicit (§11).
#[must_use]
pub fn bundle_filter(owner_pubkey: &str, agent_pubkey: &str) -> Value {
    json!({
        "kinds": [KIND_WAKER_LAUNCH_BUNDLE],
        "authors": [normalize_pubkey(owner_pubkey)],
        "#p": [normalize_pubkey(agent_pubkey)],
    })
}

/// The REQ frame opening one agent's bundle-delivery tap.
#[must_use]
pub fn bundle_req(owner_pubkey: &str, agent_pubkey: &str) -> Value {
    json!([
        "REQ",
        BUNDLE_TAP_SUBSCRIPTION_ID,
        bundle_filter(owner_pubkey, agent_pubkey)
    ])
}

/// Shared, thread-safe cache of the current admitted bundle.
///
/// This is the whole answer to "how does [`crate::wake_loop`] get a bundle
/// for [`crate::effects::RealWakeEffects`] without touching a socket
/// itself": the tap task ([`run_bundle_tap`]) owns the connection and the
/// [`FloorStore`], and writes here; everything else only reads.
#[derive(Debug, Default)]
pub struct BundleState {
    inner: Mutex<Option<Arc<LaunchBundleBody>>>,
}

impl BundleState {
    /// A tap with nothing admitted yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Recover from a poisoned lock rather than propagating it — a panic in
    /// one reader must not permanently blind every future bundle lookup for
    /// this agent.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Arc<LaunchBundleBody>>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record a newly admitted bundle as the current one.
    pub fn set(&self, body: LaunchBundleBody) {
        *self.lock() = Some(Arc::new(body));
    }

    /// The current admitted bundle, if any has been delivered and admitted
    /// on this daemon run yet.
    #[must_use]
    pub fn current(&self) -> Option<Arc<LaunchBundleBody>> {
        self.lock().clone()
    }
}

/// What one delivered relay message means for this tap.
///
/// Mirrors [`crate::presence_feed`]'s `PresenceFrame` convention: everything
/// this tap does not read collapses to [`BundleFrame::Ignored`].
#[derive(Debug, PartialEq, Eq)]
enum BundleFrame {
    /// A verified `kind:30180` delivery, authored by the pinned owner,
    /// carrying its raw (still-encrypted) content.
    Delivered { ciphertext: String },
    /// An event on this subscription that failed signature verification.
    Rejected { event_id: String, reason: String },
    /// This subscription was closed by the relay.
    Closed { message: String },
    /// A frame for a subscription this tap did not open, an event whose
    /// kind or author doesn't match (should be excluded by the filter
    /// already — checked again here as the same defense-in-depth the
    /// `authors` filter itself is), or a message type this tap has no use
    /// for.
    Ignored,
}

/// Classify one relay message for `owner_pubkey`/`agent_pubkey`'s bundle tap.
///
/// Verification proves only that the stated author signed the event — it
/// does not prove the relay applied this subscription's `kinds`/`authors`/`#p`
/// filter. Checking author here (not just the signature) is what stops a
/// misrouted or replayed event for a different owner from ever reaching the
/// decrypt step — same reasoning [`crate::presence_feed`] applies for its own
/// tap.
fn bundle_frame(owner_pubkey: &str, message: RelayMessage) -> BundleFrame {
    match message {
        RelayMessage::Event {
            subscription_id,
            event,
        } if subscription_id == BUNDLE_TAP_SUBSCRIPTION_ID => {
            if let Err(error) = buzz_core::verify_event(&event) {
                return BundleFrame::Rejected {
                    event_id: event.id.to_hex(),
                    reason: error.to_string(),
                };
            }
            if buzz_core::kind::event_kind_u32(&event) != KIND_WAKER_LAUNCH_BUNDLE {
                return BundleFrame::Ignored;
            }
            if normalize_pubkey(&event.pubkey.to_hex()) != normalize_pubkey(owner_pubkey) {
                return BundleFrame::Ignored;
            }
            BundleFrame::Delivered {
                ciphertext: event.content.clone(),
            }
        }
        RelayMessage::Closed {
            subscription_id,
            message,
        } if subscription_id == BUNDLE_TAP_SUBSCRIPTION_ID => BundleFrame::Closed { message },
        _ => BundleFrame::Ignored,
    }
}

/// Decrypt, verify, and admit one delivered bundle.
///
/// `keys` is the **agent's own** keypair (the NIP-44 recipient); the
/// ciphertext was encrypted to it. The sender side of the ECDH is
/// `owner_pubkey` — already confirmed to be the event's own `pubkey` by
/// [`bundle_frame`], which is itself confirmed to be the relay-authenticated
/// signer by ordinary ingest (this kind gets no gift-wrap exemption).
///
/// Order matters, matching [`SignedLaunchBundle::verify`]'s own doc: decrypt
/// (confidentiality) is not a trust decision; `verify` (against the
/// `FloorStore`-pinned owner) is. A decrypted-but-forged or tampered body
/// fails `verify`, never silently activates.
///
/// # Errors
/// A human-readable message on any failure — malformed/oversized ciphertext,
/// a decrypt failure, a parse failure, a failed inner signature check, or a
/// floor refusal (revoked/rolled-back version). Every path is a refusal to
/// activate, never a credential in the error text.
fn decrypt_verify_and_admit(
    keys: &Keys,
    owner_pubkey: &str,
    ciphertext: &str,
    floor_store: &mut FloorStore,
) -> Result<LaunchBundleBody, String> {
    if !NIP44_CONTENT_LEN_RANGE.contains(&ciphertext.len()) {
        return Err(format!(
            "launch bundle ciphertext outside the expected NIP-44 size range \
             ({} chars)",
            ciphertext.len()
        ));
    }
    let owner_pk = nostr::PublicKey::from_hex(owner_pubkey)
        .map_err(|error| format!("malformed owner pubkey: {error}"))?;
    let mut plaintext = nostr::nips::nip44::decrypt(keys.secret_key(), &owner_pk, ciphertext)
        .map_err(|error| format!("NIP-44 decrypt failed: {error}"))?;

    let parsed: Result<SignedLaunchBundle, _> = serde_json::from_str(&plaintext);
    plaintext.zeroize();
    let signed = parsed.map_err(|error| format!("malformed launch bundle JSON: {error}"))?;

    let pinned_owner = floor_store
        .pinned_owner()
        .map_err(|error| format!("could not read pinned owner: {error}"))?;
    let now = crate::presence_feed::now_ms() / 1000;
    let body = signed
        .verify(&pinned_owner, now)
        .map_err(|error| format!("launch bundle verification failed: {error}"))?;

    floor_store
        .admit(body.bundle_version)
        .map_err(|error| format!("launch bundle floor refused it: {error}"))?;

    Ok(body)
}

/// Run one agent's bundle-delivery tap until `cancel` fires.
///
/// Connects, authenticates as `keys` (the agent's own identity — same as the
/// mention feed and presence tap), subscribes under
/// [`BUNDLE_TAP_SUBSCRIPTION_ID`], and folds every delivery into `state` via
/// [`decrypt_verify_and_admit`]. Reconnects on any transport error using the
/// same ladder the mention feed and presence tap both use
/// ([`reconnect_delay_ms`]).
///
/// `floor_store` is owned by this task for its lifetime — the same
/// single-owner shape [`crate::cursor::CursorStore`] uses for the mention
/// feed's cursor, for the same reason: the floor's fenced persistence
/// assumes one writer.
///
/// A malformed, undecryptable, or floor-refused delivery is logged and
/// skipped, not a reconnect — an attacker (or a stale bundle replayed after
/// a legitimate reissue) publishing junk tagged to this agent must not be
/// able to knock this tap offline.
#[allow(clippy::too_many_arguments)]
pub async fn run_bundle_tap(
    relay_url: &str,
    keys: &Keys,
    auth_tag: Option<&Tag>,
    owner_pubkey: &str,
    floor_store: &mut FloorStore,
    state: &BundleState,
    cancel: &CancellationToken,
) {
    let agent_pubkey = keys.public_key().to_hex();
    let owner_pubkey = normalize_pubkey(owner_pubkey);
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
                    tracing::warn!(agent = %agent_pubkey, %error, "bundle tap connect failed; backing off");
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    continue;
                }
            },
            () = cancel.cancelled() => break,
        };

        if let Err(error) = connection
            .send_raw(&bundle_req(&owner_pubkey, &agent_pubkey))
            .await
        {
            tracing::warn!(agent = %agent_pubkey, %error, "bundle tap subscribe failed; reconnecting");
            consecutive_failures = consecutive_failures.saturating_add(1);
            continue;
        }
        consecutive_failures = 0;

        loop {
            let next = tokio::select! {
                result = connection.next_event(Duration::from_secs(BUNDLE_TAP_IDLE_TIMEOUT_SECS)) => result,
                () = cancel.cancelled() => return,
            };

            match next {
                Ok(message) => match bundle_frame(&owner_pubkey, message) {
                    BundleFrame::Delivered { ciphertext } => {
                        match decrypt_verify_and_admit(
                            keys,
                            &owner_pubkey,
                            &ciphertext,
                            floor_store,
                        ) {
                            Ok(body) => {
                                tracing::info!(
                                    agent = %agent_pubkey,
                                    bundle_version = body.bundle_version,
                                    "bundle tap admitted a launch bundle"
                                );
                                state.set(body);
                            }
                            Err(error) => {
                                tracing::warn!(
                                    agent = %agent_pubkey,
                                    %error,
                                    "bundle tap received a delivery it could not admit; ignoring"
                                );
                            }
                        }
                    }
                    BundleFrame::Rejected { event_id, reason } => {
                        tracing::warn!(
                            agent = %agent_pubkey,
                            event_id = %event_id,
                            %reason,
                            "bundle tap received an event that failed verification; ignoring"
                        );
                    }
                    BundleFrame::Closed { message } => {
                        tracing::warn!(
                            agent = %agent_pubkey,
                            %message,
                            "bundle tap subscription closed by relay; reconnecting"
                        );
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        break;
                    }
                    BundleFrame::Ignored => {}
                },
                Err(WsClientError::Timeout) => {
                    // A quiet tap is the normal case — see BUNDLE_TAP_IDLE_TIMEOUT_SECS.
                }
                Err(error) => {
                    tracing::warn!(agent = %agent_pubkey, %error, "bundle tap connection lost; reconnecting");
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
    use nostr::{EventBuilder, Kind};

    fn bundle_event(owner: &Keys, ciphertext: &str, agent_pubkey: &str) -> nostr::Event {
        EventBuilder::new(Kind::Custom(KIND_WAKER_LAUNCH_BUNDLE as u16), ciphertext)
            .tags([
                Tag::parse(["d", agent_pubkey]).unwrap(),
                Tag::parse(["p", agent_pubkey]).unwrap(),
            ])
            .sign_with_keys(owner)
            .expect("sign")
    }

    #[test]
    fn a_verified_delivery_from_the_pinned_owner_is_delivered() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let agent_pubkey = "a".repeat(64);
        let event = bundle_event(&owner, "ciphertext-bytes", &agent_pubkey);

        let frame = bundle_frame(
            &owner_pubkey,
            RelayMessage::Event {
                subscription_id: BUNDLE_TAP_SUBSCRIPTION_ID.to_string(),
                event: Box::new(event),
            },
        );
        assert_eq!(
            frame,
            BundleFrame::Delivered {
                ciphertext: "ciphertext-bytes".to_string()
            }
        );
    }

    #[test]
    fn a_delivery_from_an_unpinned_signer_is_ignored() {
        let owner = Keys::generate();
        let attacker = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let agent_pubkey = "a".repeat(64);
        // Signed by someone other than the pinned owner — the relay's own
        // ingest already refuses a forged `event.pubkey` for this kind, but
        // this tap must not trust the wire either.
        let event = bundle_event(&attacker, "ciphertext-bytes", &agent_pubkey);

        let frame = bundle_frame(
            &owner_pubkey,
            RelayMessage::Event {
                subscription_id: BUNDLE_TAP_SUBSCRIPTION_ID.to_string(),
                event: Box::new(event),
            },
        );
        assert_eq!(frame, BundleFrame::Ignored);
    }

    #[test]
    fn a_frame_for_another_subscription_is_ignored() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let event = bundle_event(&owner, "ciphertext-bytes", &"a".repeat(64));

        let frame = bundle_frame(
            &owner_pubkey,
            RelayMessage::Event {
                subscription_id: "some-other-subscription".to_string(),
                event: Box::new(event),
            },
        );
        assert_eq!(frame, BundleFrame::Ignored);
    }

    #[test]
    fn a_closed_frame_for_this_subscription_is_reported() {
        let frame = bundle_frame(
            &"a".repeat(64),
            RelayMessage::Closed {
                subscription_id: BUNDLE_TAP_SUBSCRIPTION_ID.to_string(),
                message: "auth-required".to_string(),
            },
        );
        assert_eq!(
            frame,
            BundleFrame::Closed {
                message: "auth-required".to_string()
            }
        );
    }

    #[test]
    fn oversized_ciphertext_is_refused_before_any_decrypt_attempt() {
        let agent_keys = Keys::generate();
        let owner_pubkey = "a".repeat(64);
        let dir = tempfile::tempdir().unwrap();
        let mut floor_store =
            FloorStore::enroll(dir.path().join("floor.json"), &owner_pubkey).unwrap();

        let too_long = "x".repeat(NIP44_CONTENT_LEN_RANGE.end() + 1);
        let error =
            decrypt_verify_and_admit(&agent_keys, &owner_pubkey, &too_long, &mut floor_store)
                .unwrap_err();
        assert!(error.contains("size range"), "{error}");

        let too_short = "x".repeat(NIP44_CONTENT_LEN_RANGE.start() - 1);
        let error =
            decrypt_verify_and_admit(&agent_keys, &owner_pubkey, &too_short, &mut floor_store)
                .unwrap_err();
        assert!(error.contains("size range"), "{error}");
    }

    #[test]
    fn a_valid_round_trip_decrypts_verifies_and_admits() {
        use crate::bundle::{LaunchBundleBody, ProviderEnvelope};

        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let agent = Keys::generate();
        let dir = tempfile::tempdir().unwrap();
        let mut floor_store =
            FloorStore::enroll(dir.path().join("floor.json"), &owner_pubkey).unwrap();

        let body = LaunchBundleBody {
            agent_pubkey: agent.public_key().to_hex(),
            agent_json: serde_json::json!({"launch": {"policy_env": {}}}),
            provider: ProviderEnvelope {
                provider_id: "sprites".to_string(),
                provider_config: serde_json::json!({}),
                provider_binary_sha256: "b".repeat(64),
            },
            bundle_version: 1,
            issued_at: 0,
            expires_at: u64::MAX,
            owner_only_access: true,
        };
        let owner_keypair =
            nostr::secp256k1::Keypair::from_secret_key(nostr::SECP256K1, owner.secret_key());
        let signed = SignedLaunchBundle::sign(&body, &owner_keypair).unwrap();
        let plaintext = serde_json::to_string(&signed).unwrap();
        let ciphertext = nostr::nips::nip44::encrypt(
            owner.secret_key(),
            &agent.public_key(),
            &plaintext,
            nostr::nips::nip44::Version::V2,
        )
        .unwrap();

        let admitted =
            decrypt_verify_and_admit(&agent, &owner_pubkey, &ciphertext, &mut floor_store)
                .expect("round trip");
        assert_eq!(admitted.bundle_version, 1);
    }
}
