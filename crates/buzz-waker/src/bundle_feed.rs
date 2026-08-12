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

use buzz_core::kind::KIND_WAKER_BUNDLE_ENVELOPE;
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

/// How many envelopes to ask for on subscribe.
///
/// The envelope kind is not parameterized-replaceable, so every reissue and
/// revocation lands beside its predecessors instead of replacing them, and an
/// unbounded query would grow with an agent's whole enrolment history. The
/// newest is the one that matters — anything older is at or below the
/// [`FloorStore`]'s revocation floor and would be refused anyway — so a small
/// window is enough while still leaving room to observe a revocation that
/// arrived immediately after the bundle it revokes.
const BUNDLE_QUERY_LIMIT: u32 = 16;

/// The REQ filter for one agent's bundle tap: global, `authors` pinned to
/// the enrolment-pinned owner, `#p` pinned to the target agent.
///
/// `authors` is what actually stops a flooding attacker from ever appearing
/// in this query's results — ordinary ingest already refuses a forged
/// `event.pubkey`, so `authors` here is redundant with what the relay
/// enforces at write time, kept as defense in depth and to make the query's
/// own intent explicit (§11). It matters more under the envelope kind than it
/// did under a dedicated one: the envelope is a kind any member may write, so
/// `authors` is what keeps this query from surfacing anyone else's traffic to
/// this agent.
///
/// `#p` is not optional. The relay refuses an envelope query that does not
/// carry one (`push_filter_authorized_for_event`'s read-path counterpart), so
/// a filter without it is closed rather than answered.
#[must_use]
pub fn bundle_filter(owner_pubkey: &str, agent_pubkey: &str) -> Value {
    json!({
        "kinds": [KIND_WAKER_BUNDLE_ENVELOPE],
        "authors": [normalize_pubkey(owner_pubkey)],
        "#p": [normalize_pubkey(agent_pubkey)],
        "limit": BUNDLE_QUERY_LIMIT,
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

    /// Drop whatever bundle is currently held, in response to an owner-signed
    /// revocation. Leaves this daemon with nothing to deploy until a fresh,
    /// non-revoked bundle is delivered.
    pub fn clear(&self) {
        *self.lock() = None;
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
    /// A verified envelope delivery, authored by the pinned owner,
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
            if buzz_core::kind::event_kind_u32(&event) != KIND_WAKER_BUNDLE_ENVELOPE {
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

/// What a decrypted, verified delivery means for the tap's caller.
#[derive(Debug, PartialEq)]
enum BundleOutcome {
    /// A launch bundle to hold as the current one.
    Delivered(LaunchBundleBody),
    /// An owner-signed revocation. The revocation floor has already been
    /// raised (durably, best-effort) by the time this is returned; the
    /// caller's only remaining job is to drop whatever it was holding.
    Revoked,
}

/// Decrypt, verify, and admit (or revoke) one delivered bundle.
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
/// A revocation (`LaunchBundleBody::revoked`) is delivered on the exact same
/// wire path as a real bundle — same coordinate, same subscription, same
/// decrypt/verify — so it reaches an already-connected daemon exactly as
/// promptly as a config-change reissue does. It raises the floor rather than
/// admitting a bundle, and never touches `agent_json`/`provider`, which the
/// issuer leaves as unused placeholders for a revocation.
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
) -> Result<BundleOutcome, String> {
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

    // `verify` only checks the owner's signature over the body — it does not,
    // and by design cannot, know which agent this daemon is running as. An
    // owner-signed bundle whose `agent_pubkey` names a *different* agent must
    // never reach `admit`: that would durably raise this agent's version
    // floor (and replace its live cache) for a bundle that was never meant
    // for it, poisoning both until manual state repair.
    let receiving_agent = normalize_pubkey(&keys.public_key().to_hex());
    if normalize_pubkey(&body.agent_pubkey) != receiving_agent {
        return Err(format!(
            "launch bundle targets agent {}, not the receiving agent {receiving_agent}",
            body.agent_pubkey
        ));
    }

    if body.revoked {
        if let Err(error) = floor_store.raise_revocation_floor(body.bundle_version) {
            // Fail closed on the in-memory side even if the durable floor
            // didn't move: a revocation the daemon cannot persist must not
            // leave it still willing to deploy from its live cache. Worst
            // case if this daemon then restarts before ever hearing another
            // delivery, `FloorStore` reopens at the old floor — the
            // pre-existing G2 rollback surface, not a new one.
            tracing::warn!(
                agent = %normalize_pubkey(&keys.public_key().to_hex()),
                %error,
                "bundle tap could not durably raise the revocation floor; revoking this run's cache anyway"
            );
        }
        return Ok(BundleOutcome::Revoked);
    }

    floor_store
        .admit(body.bundle_version)
        .map_err(|error| format!("launch bundle floor refused it: {error}"))?;

    Ok(BundleOutcome::Delivered(body))
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
                            Ok(BundleOutcome::Delivered(body)) => {
                                tracing::info!(
                                    agent = %agent_pubkey,
                                    bundle_version = body.bundle_version,
                                    "bundle tap admitted a launch bundle"
                                );
                                state.set(body);
                            }
                            Ok(BundleOutcome::Revoked) => {
                                tracing::info!(
                                    agent = %agent_pubkey,
                                    "bundle tap received a revocation; clearing the cached bundle"
                                );
                                state.clear();
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
        EventBuilder::new(Kind::Custom(KIND_WAKER_BUNDLE_ENVELOPE as u16), ciphertext)
            .tags([
                Tag::parse(["d", agent_pubkey]).unwrap(),
                Tag::parse(["p", agent_pubkey]).unwrap(),
            ])
            .sign_with_keys(owner)
            .expect("sign")
    }

    /// The transport regression. A bundle published under the payload kind is
    /// refused at ingest by any relay that has not adopted it — which is what
    /// made remote wake silently impossible, since the desktop discards the
    /// rejection and this tap simply never receives anything. The query must
    /// name the envelope kind, and must carry `#p`, which the relay requires
    /// for this kind rather than answering an unscoped query.
    #[test]
    fn the_query_asks_for_the_envelope_kind_not_the_payload_kind() {
        let owner_pubkey = "b".repeat(64);
        let agent_pubkey = "a".repeat(64);
        let filter = bundle_filter(&owner_pubkey, &agent_pubkey);

        assert_eq!(
            filter["kinds"],
            json!([KIND_WAKER_BUNDLE_ENVELOPE]),
            "publishing under the payload kind is what the relay rejects"
        );
        assert_ne!(
            filter["kinds"],
            json!([buzz_core::kind::KIND_WAKER_LAUNCH_BUNDLE]),
            "the payload kind must never reach the wire"
        );
        assert_eq!(filter["#p"], json!([agent_pubkey]), "#p is not optional");
        assert_eq!(filter["authors"], json!([owner_pubkey]));
        assert!(
            filter["limit"].is_number(),
            "the envelope is not replaceable, so the query must be bounded"
        );
    }

    /// An envelope carrying the payload kind's number must not be mistaken for
    /// a delivery: the tap keys off the envelope kind alone.
    #[test]
    fn an_event_under_the_payload_kind_is_ignored() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let agent_pubkey = "a".repeat(64);
        let event = EventBuilder::new(
            Kind::Custom(buzz_core::kind::KIND_WAKER_LAUNCH_BUNDLE as u16),
            "ciphertext-bytes",
        )
        .tags([
            Tag::parse(["d", &agent_pubkey]).unwrap(),
            Tag::parse(["p", &agent_pubkey]).unwrap(),
        ])
        .sign_with_keys(&owner)
        .expect("sign");

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
            revoked: false,
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
        assert_eq!(
            admitted,
            BundleOutcome::Delivered(
                signed
                    .verify(&owner_pubkey, u64::MAX)
                    .expect("the same body the tap just admitted")
            )
        );
        assert_eq!(floor_store.snapshot().highest_accepted_version, 1);
    }

    /// The headline case this whole outcome type exists for: a revocation
    /// raises the floor instead of admitting a bundle, and reports
    /// `Revoked` so the caller clears its cache.
    #[test]
    fn a_revocation_raises_the_floor_and_reports_revoked() {
        use crate::bundle::{LaunchBundleBody, ProviderEnvelope};

        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let agent = Keys::generate();
        let dir = tempfile::tempdir().unwrap();
        let mut floor_store =
            FloorStore::enroll(dir.path().join("floor.json"), &owner_pubkey).unwrap();

        let body = LaunchBundleBody {
            agent_pubkey: agent.public_key().to_hex(),
            // Placeholders — a revoked delivery must never be read this far.
            agent_json: serde_json::Value::Null,
            provider: ProviderEnvelope {
                provider_id: String::new(),
                provider_config: serde_json::json!({}),
                provider_binary_sha256: String::new(),
            },
            bundle_version: 5,
            issued_at: 0,
            expires_at: u64::MAX,
            owner_only_access: true,
            revoked: true,
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

        let outcome =
            decrypt_verify_and_admit(&agent, &owner_pubkey, &ciphertext, &mut floor_store)
                .expect("a revocation is not an error");
        assert_eq!(outcome, BundleOutcome::Revoked);
        assert_eq!(floor_store.snapshot().revocation_floor, 5);

        // A later delivery below the raised floor is refused, same as any
        // other revoked version.
        let mut stale = body.clone();
        stale.bundle_version = 3;
        stale.revoked = false;
        let stale_signed = SignedLaunchBundle::sign(&stale, &owner_keypair).unwrap();
        let stale_plaintext = serde_json::to_string(&stale_signed).unwrap();
        let stale_ciphertext = nostr::nips::nip44::encrypt(
            owner.secret_key(),
            &agent.public_key(),
            &stale_plaintext,
            nostr::nips::nip44::Version::V2,
        )
        .unwrap();
        let error =
            decrypt_verify_and_admit(&agent, &owner_pubkey, &stale_ciphertext, &mut floor_store)
                .expect_err("must be refused as revoked");
        assert!(error.contains("floor refused"), "{error}");
    }

    #[test]
    fn a_bundle_targeting_another_agent_is_refused_and_does_not_advance_the_floor() {
        use crate::bundle::{LaunchBundleBody, ProviderEnvelope};

        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        // The receiving agent and the bundle's declared target are different
        // keys — an owner-signed bundle correctly addressed (NIP-44, `#p`) to
        // `agent` but whose signed body names `other_agent`.
        let agent = Keys::generate();
        let other_agent = Keys::generate();
        let dir = tempfile::tempdir().unwrap();
        let mut floor_store =
            FloorStore::enroll(dir.path().join("floor.json"), &owner_pubkey).unwrap();

        let body = LaunchBundleBody {
            agent_pubkey: other_agent.public_key().to_hex(),
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
            revoked: false,
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

        let error = decrypt_verify_and_admit(&agent, &owner_pubkey, &ciphertext, &mut floor_store)
            .expect_err("must be refused");
        assert!(error.contains("targets agent"), "{error}");
        assert_eq!(floor_store.snapshot().highest_accepted_version, 0);
    }
}
