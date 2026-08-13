//! The per-agent credential-delivery tap — daemon-side counterpart to
//! [`crate::roster_feed`] (`docs/waker-agent-enrolment.md`,
//! `PLANS/BUZZ_WAKER_DESIGN.md` §12, build order step 2).
//!
//! Shape closely mirrors [`crate::bundle_feed`] — same connect/backoff/
//! idle-timeout machinery, same decrypt-then-verify-then-admit split against
//! a per-agent [`FloorStore`] — with two deliberate differences the design
//! doc's Per-agent credential delivery section calls for:
//!
//! - **Connects and decrypts as the daemon's own waker identity**, not the
//!   target agent's. A credential tap's whole purpose is delivering an
//!   agent's own `nsec` to the daemon *before* the daemon has it — there is
//!   no agent identity to authenticate as yet. [`crate::roster_feed`] shares
//!   this same reasoning.
//! - **The query adds `#d` pinned to the target agent's pubkey.** A bundle
//!   tap's `#p` alone disambiguates one agent from another because `#p` is
//!   that agent's own identity; here `#p` is the waker's identity, shared
//!   across every agent one owner enrols, so `#d` is what disambiguates.
//!   [`credential_frame`] re-checks it per delivered frame as defense in
//!   depth, the same reasoning [`crate::roster_feed::roster_frame`] applies
//!   to its own fixed `#d`.
//!
//! # What this tap does *not* do
//!
//! Decide whether a brand-new agent should be trusted at all, or supervise
//! anything. The caller is responsible for constructing the
//! [`FloorStore`] this tap admits against — for an agent this daemon has
//! never seen, that means enrolling one with an owner pubkey already proven
//! against `WAKER_OWNER_PUBKEYS` (typically via an already-verified roster
//! entry), *before* calling [`run_credential_tap`]. This module only wires an
//! already-decided trust anchor to a live socket, exactly the split
//! [`crate::bundle_feed`] already keeps.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use buzz_core::kind::KIND_WAKER_BUNDLE_ENVELOPE;
use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};
use nostr::{Keys, Tag};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroize;

use crate::bundle_feed::NIP44_CONTENT_LEN_RANGE;
use crate::decide::normalize_pubkey;
use crate::enrolment::{CredentialBody, SignedCredential};
use crate::feed::reconnect_delay_ms;
use crate::floors::FloorStore;

/// Subscription id for one agent's credential-delivery tap. Fixed, like every
/// other tap's own id — a reconnect replaces the old subscription rather than
/// piling up a fresh one.
pub const CREDENTIAL_TAP_SUBSCRIPTION_ID: &str = "buzz-waker-credential";

/// How long to wait for a frame before treating the tap connection as idle.
///
/// Matches [`crate::bundle_feed::BUNDLE_TAP_IDLE_TIMEOUT_SECS`]'s own
/// reasoning: a credential is reissued on rotation or config change only,
/// never as a liveness ping.
pub const CREDENTIAL_TAP_IDLE_TIMEOUT_SECS: u64 = 300;

/// How many envelopes to ask for on subscribe. Same value and reasoning as
/// [`crate::bundle_feed::BUNDLE_QUERY_LIMIT`] / [`crate::roster_feed::ROSTER_QUERY_LIMIT`].
const CREDENTIAL_QUERY_LIMIT: u32 = 16;

/// The REQ filter for one agent's credential tap: global, `authors` pinned to
/// the enrolment-pinned owner, `#p` pinned to the **waker's own** identity,
/// `#d` pinned to the target agent's pubkey.
#[must_use]
pub fn credential_filter(owner_pubkey: &str, waker_pubkey: &str, agent_pubkey: &str) -> Value {
    json!({
        "kinds": [KIND_WAKER_BUNDLE_ENVELOPE],
        "authors": [normalize_pubkey(owner_pubkey)],
        "#p": [normalize_pubkey(waker_pubkey)],
        "#d": [normalize_pubkey(agent_pubkey)],
        "limit": CREDENTIAL_QUERY_LIMIT,
    })
}

/// The REQ frame opening one agent's credential-delivery tap.
#[must_use]
pub fn credential_req(owner_pubkey: &str, waker_pubkey: &str, agent_pubkey: &str) -> Value {
    json!([
        "REQ",
        CREDENTIAL_TAP_SUBSCRIPTION_ID,
        credential_filter(owner_pubkey, waker_pubkey, agent_pubkey)
    ])
}

/// Shared, thread-safe cache of one agent's current admitted credential.
///
/// Mirrors [`crate::bundle_feed::BundleState`]'s shape exactly, one instance
/// per watched agent — the tap task ([`run_credential_tap`]) owns the
/// connection and the [`FloorStore`], and writes here; everything else only
/// reads.
#[derive(Debug, Default)]
pub struct CredentialState {
    inner: Mutex<Option<Arc<CredentialBody>>>,
}

impl CredentialState {
    /// A tap with nothing admitted yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Recover from a poisoned lock rather than propagating it — a panic in
    /// one reader must not permanently blind every future credential lookup
    /// for this agent.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Arc<CredentialBody>>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record a newly admitted credential as the current one.
    pub fn set(&self, body: CredentialBody) {
        *self.lock() = Some(Arc::new(body));
    }

    /// Drop whatever credential is currently held, in response to an
    /// owner-signed revocation.
    pub fn clear(&self) {
        *self.lock() = None;
    }

    /// The current admitted credential, if any has been delivered and
    /// admitted on this daemon run yet.
    #[must_use]
    pub fn current(&self) -> Option<Arc<CredentialBody>> {
        self.lock().clone()
    }
}

/// What one delivered relay message means for this tap.
///
/// Mirrors [`crate::bundle_feed`]'s own `BundleFrame` convention: everything
/// this tap does not read collapses to [`CredentialFrame::Ignored`].
#[derive(Debug, PartialEq, Eq)]
enum CredentialFrame {
    /// A verified envelope delivery, authored by the pinned owner and tagged
    /// for the target agent, carrying its raw (still-encrypted) content.
    Delivered { ciphertext: String },
    /// An event on this subscription that failed signature verification.
    Rejected { event_id: String, reason: String },
    /// This subscription was closed by the relay.
    Closed { message: String },
    /// A frame for a subscription this tap did not open, an event whose
    /// kind/`#d`/author doesn't match (should be excluded by the filter
    /// already — checked again here as defense in depth), or a message type
    /// this tap has no use for.
    Ignored,
}

/// Classify one relay message for `owner_pubkey`/`agent_pubkey`'s credential
/// tap.
///
/// Re-checking `#d` against `agent_pubkey` here is what stops a misrouted or
/// replayed event for a *different* agent under the same owner+waker from
/// ever reaching the decrypt step — the roster's fixed sentinel can never
/// collide with a real agent pubkey (see [`crate::roster_feed`]'s module
/// doc), but two different agents' own `#d` values are both ordinary
/// canonical pubkeys and could otherwise be confused by a filter bug.
fn credential_frame(
    owner_pubkey: &str,
    agent_pubkey: &str,
    message: RelayMessage,
) -> CredentialFrame {
    match message {
        RelayMessage::Event {
            subscription_id,
            event,
        } if subscription_id == CREDENTIAL_TAP_SUBSCRIPTION_ID => {
            if let Err(error) = buzz_core::verify_event(&event) {
                return CredentialFrame::Rejected {
                    event_id: event.id.to_hex(),
                    reason: error.to_string(),
                };
            }
            if buzz_core::kind::event_kind_u32(&event) != KIND_WAKER_BUNDLE_ENVELOPE {
                return CredentialFrame::Ignored;
            }
            if event.tags.identifier().map(normalize_pubkey) != Some(normalize_pubkey(agent_pubkey))
            {
                return CredentialFrame::Ignored;
            }
            if normalize_pubkey(&event.pubkey.to_hex()) != normalize_pubkey(owner_pubkey) {
                return CredentialFrame::Ignored;
            }
            CredentialFrame::Delivered {
                ciphertext: event.content.clone(),
            }
        }
        RelayMessage::Closed {
            subscription_id,
            message,
        } if subscription_id == CREDENTIAL_TAP_SUBSCRIPTION_ID => {
            CredentialFrame::Closed { message }
        }
        _ => CredentialFrame::Ignored,
    }
}

/// What a decrypted, verified delivery means for the tap's caller.
///
/// Mirrors [`crate::bundle_feed`]'s own `BundleOutcome` — same three cases,
/// same reasoning for each, adapted to a credential's `credential_version`.
#[derive(Debug, PartialEq)]
enum CredentialOutcome {
    /// A credential to hold as the current one.
    Delivered(CredentialBody),
    /// An owner-signed revocation. The revocation floor has already been
    /// raised (durably, best-effort) by the time this is returned; the
    /// caller's only remaining job is to drop whatever it was holding.
    Revoked,
    /// An owner-signed revocation whose version is below the version already
    /// admitted — a later, still-valid reissue has already superseded it.
    /// See [`crate::bundle_feed`]'s own `BundleOutcome::StaleRevocation` doc
    /// for why the currently held credential must not be cleared here.
    StaleRevocation,
}

/// Decrypt, verify, and admit (or revoke) one delivered credential.
///
/// `waker_keys` is the **daemon's own** keypair (the NIP-44 recipient); the
/// ciphertext was encrypted to it — not the target agent's own keys, which
/// this tap does not have yet. The sender side of the ECDH is `owner_pubkey`
/// — already confirmed to be the event's own `pubkey` by [`credential_frame`],
/// which is itself confirmed to be the relay-authenticated signer by
/// ordinary ingest (this kind gets no gift-wrap exemption).
///
/// Order matches [`crate::bundle_feed::decrypt_verify_and_admit`]'s own doc:
/// decrypt (confidentiality) is not a trust decision; `verify` (against the
/// `FloorStore`-pinned owner) is.
///
/// # Errors
/// A human-readable message on any failure — malformed/oversized ciphertext,
/// a decrypt failure, a parse failure, a failed inner signature check, or a
/// floor refusal (revoked/rolled-back version). Every path is a refusal to
/// admit, never a credential in the error text.
fn decrypt_verify_and_admit(
    waker_keys: &Keys,
    owner_pubkey: &str,
    agent_pubkey: &str,
    ciphertext: &str,
    floor_store: &mut FloorStore,
) -> Result<CredentialOutcome, String> {
    if !NIP44_CONTENT_LEN_RANGE.contains(&ciphertext.len()) {
        return Err(format!(
            "credential ciphertext outside the expected NIP-44 size range ({} chars)",
            ciphertext.len()
        ));
    }
    let owner_pk = nostr::PublicKey::from_hex(owner_pubkey)
        .map_err(|error| format!("malformed owner pubkey: {error}"))?;
    let mut plaintext = nostr::nips::nip44::decrypt(waker_keys.secret_key(), &owner_pk, ciphertext)
        .map_err(|error| format!("NIP-44 decrypt failed: {error}"))?;

    let parsed: Result<SignedCredential, _> = serde_json::from_str(&plaintext);
    plaintext.zeroize();
    let signed = parsed.map_err(|error| format!("malformed credential JSON: {error}"))?;

    let pinned_owner = floor_store
        .pinned_owner()
        .map_err(|error| format!("could not read pinned owner: {error}"))?;
    let body = signed
        .verify(&[pinned_owner])
        .map_err(|error| format!("credential verification failed: {error}"))?;

    // `verify` only checks the owner's signature over the body — it does
    // not, and by design cannot, know which agent this tap instance is
    // watching. An owner-signed credential whose `agent_pubkey` names a
    // *different* agent must never reach `admit`: that would durably raise
    // this agent's version floor for a credential that was never meant for
    // it. Mirrors bundle_feed's own `agent_pubkey` check exactly, except the
    // receiving identity here is the parameter, not `keys.public_key()` —
    // this tap authenticates as the waker, not the agent it watches.
    let target_agent = normalize_pubkey(agent_pubkey);
    if normalize_pubkey(&body.agent_pubkey) != target_agent {
        return Err(format!(
            "credential targets agent {}, not the watched agent {target_agent}",
            body.agent_pubkey
        ));
    }

    if body.revoked {
        if let Err(error) = floor_store.raise_revocation_floor(body.credential_version) {
            tracing::warn!(
                agent = %target_agent,
                %error,
                "credential tap could not durably raise the revocation floor; revoking this run's cache anyway"
            );
        }

        if body.credential_version < floor_store.snapshot().highest_accepted_version {
            return Ok(CredentialOutcome::StaleRevocation);
        }
        return Ok(CredentialOutcome::Revoked);
    }

    floor_store
        .admit(body.credential_version)
        .map_err(|error| format!("credential floor refused it: {error}"))?;

    Ok(CredentialOutcome::Delivered(body))
}

/// Run one agent's credential-delivery tap until `cancel` fires.
///
/// Connects and authenticates as `waker_keys` — **not** `agent_pubkey`, see
/// the module doc — subscribes under [`CREDENTIAL_TAP_SUBSCRIPTION_ID`], and
/// folds every delivery into `state` via [`decrypt_verify_and_admit`].
/// Reconnects on any transport error using the same ladder every other tap in
/// this daemon uses ([`reconnect_delay_ms`]).
///
/// `floor_store` is owned by this task for its lifetime, same single-owner
/// shape [`crate::bundle_feed::run_bundle_tap`] uses for its own floor — the
/// caller is responsible for having already enrolled or opened it against a
/// trust anchor proven before this function is called (see the module doc).
///
/// A malformed, undecryptable, or floor-refused delivery is logged and
/// skipped, not a reconnect.
#[allow(clippy::too_many_arguments)]
pub async fn run_credential_tap(
    relay_url: &str,
    waker_keys: &Keys,
    auth_tag: Option<&Tag>,
    owner_pubkey: &str,
    agent_pubkey: &str,
    floor_store: &mut FloorStore,
    state: &CredentialState,
    cancel: &CancellationToken,
) {
    let waker_pubkey = waker_keys.public_key().to_hex();
    let owner_pubkey = normalize_pubkey(owner_pubkey);
    let agent_pubkey = normalize_pubkey(agent_pubkey);
    let mut consecutive_failures = 0u32;

    while !cancel.is_cancelled() {
        if consecutive_failures > 0 {
            let delay_ms = reconnect_delay_ms(consecutive_failures);
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                () = cancel.cancelled() => break,
            }
        }

        let connect = NostrWsConnection::connect_authenticated(relay_url, waker_keys, auth_tag);
        let mut connection = tokio::select! {
            result = connect => match result {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::warn!(agent = %agent_pubkey, %error, "credential tap connect failed; backing off");
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    continue;
                }
            },
            () = cancel.cancelled() => break,
        };

        if let Err(error) = connection
            .send_raw(&credential_req(&owner_pubkey, &waker_pubkey, &agent_pubkey))
            .await
        {
            tracing::warn!(agent = %agent_pubkey, %error, "credential tap subscribe failed; reconnecting");
            consecutive_failures = consecutive_failures.saturating_add(1);
            continue;
        }
        consecutive_failures = 0;

        loop {
            let next = tokio::select! {
                result = connection.next_event(Duration::from_secs(CREDENTIAL_TAP_IDLE_TIMEOUT_SECS)) => result,
                () = cancel.cancelled() => return,
            };

            match next {
                Ok(message) => match credential_frame(&owner_pubkey, &agent_pubkey, message) {
                    CredentialFrame::Delivered { ciphertext } => {
                        match decrypt_verify_and_admit(
                            waker_keys,
                            &owner_pubkey,
                            &agent_pubkey,
                            &ciphertext,
                            floor_store,
                        ) {
                            Ok(CredentialOutcome::Delivered(body)) => {
                                tracing::info!(
                                    agent = %agent_pubkey,
                                    credential_version = body.credential_version,
                                    "credential tap admitted a credential"
                                );
                                state.set(body);
                            }
                            Ok(CredentialOutcome::Revoked) => {
                                tracing::info!(
                                    agent = %agent_pubkey,
                                    "credential tap received a revocation; clearing the cached credential"
                                );
                                state.clear();
                            }
                            Ok(CredentialOutcome::StaleRevocation) => {
                                tracing::info!(
                                    agent = %agent_pubkey,
                                    "credential tap received a revocation already superseded by a newer admitted credential; leaving the cache in place"
                                );
                            }
                            Err(error) => {
                                tracing::warn!(
                                    agent = %agent_pubkey,
                                    %error,
                                    "credential tap received a delivery it could not admit; ignoring"
                                );
                            }
                        }
                    }
                    CredentialFrame::Rejected { event_id, reason } => {
                        tracing::warn!(
                            agent = %agent_pubkey,
                            event_id = %event_id,
                            %reason,
                            "credential tap received an event that failed verification; ignoring"
                        );
                    }
                    CredentialFrame::Closed { message } => {
                        tracing::warn!(
                            agent = %agent_pubkey,
                            %message,
                            "credential tap subscription closed by relay; reconnecting"
                        );
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        break;
                    }
                    CredentialFrame::Ignored => {}
                },
                Err(WsClientError::Timeout) => {
                    // A quiet tap is the normal case — see CREDENTIAL_TAP_IDLE_TIMEOUT_SECS.
                }
                Err(error) => {
                    tracing::warn!(agent = %agent_pubkey, %error, "credential tap connection lost; reconnecting");
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
    use nostr::{EventBuilder, Kind, ToBech32};

    fn credential_event(owner: &Keys, ciphertext: &str, agent_pubkey: &str) -> nostr::Event {
        EventBuilder::new(Kind::Custom(KIND_WAKER_BUNDLE_ENVELOPE as u16), ciphertext)
            .tags([
                Tag::parse(["d", agent_pubkey]).unwrap(),
                Tag::parse(["p", &"w".repeat(64)]).unwrap(),
            ])
            .sign_with_keys(owner)
            .expect("sign")
    }

    fn credential_body(agent_pubkey: &str, nsec: &str, credential_version: u64) -> CredentialBody {
        CredentialBody {
            agent_pubkey: agent_pubkey.to_string(),
            nsec: nsec.to_string(),
            auth_tag: None,
            provider_credential: None,
            credential_version,
            issued_at: 1_000,
            revoked: false,
        }
    }

    #[test]
    fn the_query_pins_p_to_the_waker_and_d_to_the_agent() {
        let owner_pubkey = "a".repeat(64);
        let waker_pubkey = "b".repeat(64);
        let agent_pubkey = "c".repeat(64);
        let filter = credential_filter(&owner_pubkey, &waker_pubkey, &agent_pubkey);

        assert_eq!(filter["kinds"], json!([KIND_WAKER_BUNDLE_ENVELOPE]));
        assert_eq!(filter["authors"], json!([owner_pubkey]));
        assert_eq!(
            filter["#p"],
            json!([waker_pubkey]),
            "#p must be the waker's own identity, not the agent's"
        );
        assert_eq!(filter["#d"], json!([agent_pubkey]));
        assert!(
            filter["limit"].is_number(),
            "the envelope is not replaceable, so the query must be bounded"
        );
    }

    #[test]
    fn a_verified_delivery_from_the_pinned_owner_tagged_for_this_agent_is_delivered() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let agent_pubkey = "a".repeat(64);
        let event = credential_event(&owner, "ciphertext-bytes", &agent_pubkey);

        let frame = credential_frame(
            &owner_pubkey,
            &agent_pubkey,
            RelayMessage::Event {
                subscription_id: CREDENTIAL_TAP_SUBSCRIPTION_ID.to_string(),
                event: Box::new(event),
            },
        );
        assert_eq!(
            frame,
            CredentialFrame::Delivered {
                ciphertext: "ciphertext-bytes".to_string()
            }
        );
    }

    /// The whole reason `#d` re-checking exists for this tap: two different
    /// agents under the same owner+waker must not be confused with each
    /// other, unlike the roster's fixed sentinel which can never collide
    /// with either.
    #[test]
    fn a_delivery_tagged_for_a_different_agent_is_ignored() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let watched_agent = "a".repeat(64);
        let other_agent = "b".repeat(64);
        let event = credential_event(&owner, "ciphertext-bytes", &other_agent);

        let frame = credential_frame(
            &owner_pubkey,
            &watched_agent,
            RelayMessage::Event {
                subscription_id: CREDENTIAL_TAP_SUBSCRIPTION_ID.to_string(),
                event: Box::new(event),
            },
        );
        assert_eq!(frame, CredentialFrame::Ignored);
    }

    #[test]
    fn a_delivery_from_an_unpinned_signer_is_ignored() {
        let owner = Keys::generate();
        let attacker = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let agent_pubkey = "a".repeat(64);
        let event = credential_event(&attacker, "ciphertext-bytes", &agent_pubkey);

        let frame = credential_frame(
            &owner_pubkey,
            &agent_pubkey,
            RelayMessage::Event {
                subscription_id: CREDENTIAL_TAP_SUBSCRIPTION_ID.to_string(),
                event: Box::new(event),
            },
        );
        assert_eq!(frame, CredentialFrame::Ignored);
    }

    #[test]
    fn a_frame_for_another_subscription_is_ignored() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let agent_pubkey = "a".repeat(64);
        let event = credential_event(&owner, "ciphertext-bytes", &agent_pubkey);

        let frame = credential_frame(
            &owner_pubkey,
            &agent_pubkey,
            RelayMessage::Event {
                subscription_id: "some-other-subscription".to_string(),
                event: Box::new(event),
            },
        );
        assert_eq!(frame, CredentialFrame::Ignored);
    }

    #[test]
    fn a_closed_frame_for_this_subscription_is_reported() {
        let frame = credential_frame(
            &"a".repeat(64),
            &"b".repeat(64),
            RelayMessage::Closed {
                subscription_id: CREDENTIAL_TAP_SUBSCRIPTION_ID.to_string(),
                message: "auth-required".to_string(),
            },
        );
        assert_eq!(
            frame,
            CredentialFrame::Closed {
                message: "auth-required".to_string()
            }
        );
    }

    #[test]
    fn oversized_ciphertext_is_refused_before_any_decrypt_attempt() {
        let waker = Keys::generate();
        let owner_pubkey = "a".repeat(64);
        let agent_pubkey = "b".repeat(64);
        let dir = tempfile::tempdir().unwrap();
        let mut floor_store =
            FloorStore::enroll(dir.path().join("floor.json"), &owner_pubkey).unwrap();

        let too_long = "x".repeat(NIP44_CONTENT_LEN_RANGE.end() + 1);
        let error = decrypt_verify_and_admit(
            &waker,
            &owner_pubkey,
            &agent_pubkey,
            &too_long,
            &mut floor_store,
        )
        .unwrap_err();
        assert!(error.contains("size range"), "{error}");
    }

    #[test]
    fn a_valid_round_trip_decrypts_verifies_and_admits() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let waker = Keys::generate();
        let agent = Keys::generate();
        let dir = tempfile::tempdir().unwrap();
        let mut floor_store =
            FloorStore::enroll(dir.path().join("floor.json"), &owner_pubkey).unwrap();

        let body = credential_body(
            &agent.public_key().to_hex(),
            &agent.secret_key().to_bech32().unwrap(),
            1,
        );
        let owner_keypair =
            nostr::secp256k1::Keypair::from_secret_key(nostr::SECP256K1, owner.secret_key());
        let signed = SignedCredential::sign(&body, &owner_keypair).unwrap();
        let plaintext = serde_json::to_string(&signed).unwrap();
        let ciphertext = nostr::nips::nip44::encrypt(
            owner.secret_key(),
            &waker.public_key(),
            &plaintext,
            nostr::nips::nip44::Version::V2,
        )
        .unwrap();

        let admitted = decrypt_verify_and_admit(
            &waker,
            &owner_pubkey,
            &agent.public_key().to_hex(),
            &ciphertext,
            &mut floor_store,
        )
        .expect("round trip");
        assert_eq!(
            admitted,
            CredentialOutcome::Delivered(
                signed
                    .verify(&[owner_pubkey])
                    .expect("the same body the tap just admitted")
            )
        );
        assert_eq!(floor_store.snapshot().highest_accepted_version, 1);
    }

    #[test]
    fn a_revocation_raises_the_floor_and_reports_revoked() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let waker = Keys::generate();
        let agent = Keys::generate();
        let dir = tempfile::tempdir().unwrap();
        let mut floor_store =
            FloorStore::enroll(dir.path().join("floor.json"), &owner_pubkey).unwrap();

        let body = CredentialBody {
            agent_pubkey: agent.public_key().to_hex(),
            nsec: String::new(),
            auth_tag: None,
            provider_credential: None,
            credential_version: 5,
            issued_at: 0,
            revoked: true,
        };
        let owner_keypair =
            nostr::secp256k1::Keypair::from_secret_key(nostr::SECP256K1, owner.secret_key());
        let signed = SignedCredential::sign(&body, &owner_keypair).unwrap();
        let plaintext = serde_json::to_string(&signed).unwrap();
        let ciphertext = nostr::nips::nip44::encrypt(
            owner.secret_key(),
            &waker.public_key(),
            &plaintext,
            nostr::nips::nip44::Version::V2,
        )
        .unwrap();

        let outcome = decrypt_verify_and_admit(
            &waker,
            &owner_pubkey,
            &agent.public_key().to_hex(),
            &ciphertext,
            &mut floor_store,
        )
        .expect("a revocation is not an error");
        assert_eq!(outcome, CredentialOutcome::Revoked);
        assert_eq!(floor_store.snapshot().revocation_floor, 5);
    }

    #[test]
    fn a_credential_targeting_another_agent_is_refused_and_does_not_advance_the_floor() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let waker = Keys::generate();
        let watched_agent = Keys::generate();
        let other_agent = Keys::generate();
        let dir = tempfile::tempdir().unwrap();
        let mut floor_store =
            FloorStore::enroll(dir.path().join("floor.json"), &owner_pubkey).unwrap();

        let body = credential_body(
            &other_agent.public_key().to_hex(),
            &other_agent.secret_key().to_bech32().unwrap(),
            1,
        );
        let owner_keypair =
            nostr::secp256k1::Keypair::from_secret_key(nostr::SECP256K1, owner.secret_key());
        let signed = SignedCredential::sign(&body, &owner_keypair).unwrap();
        let plaintext = serde_json::to_string(&signed).unwrap();
        let ciphertext = nostr::nips::nip44::encrypt(
            owner.secret_key(),
            &waker.public_key(),
            &plaintext,
            nostr::nips::nip44::Version::V2,
        )
        .unwrap();

        let error = decrypt_verify_and_admit(
            &waker,
            &owner_pubkey,
            &watched_agent.public_key().to_hex(),
            &ciphertext,
            &mut floor_store,
        )
        .expect_err("must be refused");
        assert!(error.contains("targets agent"), "{error}");
        assert_eq!(floor_store.snapshot().highest_accepted_version, 0);
    }
}
