//! The roster tap — daemon-side discovery half of agent enrolment
//! (`docs/waker-agent-enrolment.md`, `PLANS/BUZZ_WAKER_DESIGN.md` §12, build
//! order step 2).
//!
//! One connection for the whole daemon, authenticated as **the waker's own
//! identity** (`WAKER_IDENTITY_NSEC`) — not per agent. That is the one place
//! this tap's shape genuinely diverges from [`crate::bundle_feed`]'s "one
//! connection per watched agent, authenticated as that agent": a roster's
//! whole job is telling the daemon which agents exist in the first place, so
//! there is no agent identity to authenticate as yet. [`crate::credential_feed`]
//! shares this same waker-identity connection shape for the same reason —
//! see its own module doc.
//!
//! # Discovery, not delivery
//!
//! [`crate::enrolment::RosterBody`] lists membership; it never carries a
//! secret. This tap's only job is producing the latest known roster per
//! owner in [`RosterState`] for a caller (the eventual dynamic supervisor,
//! `PLANS/BUZZ_WAKER_DESIGN.md` §12 build order step 3, not yet written) to
//! diff against the daemon's current watch list and act on. It does not
//! spawn, cancel, or otherwise supervise anything itself.
//!
//! # Multiple owners, one query
//!
//! Unlike a bundle or credential tap (pinned to one already-known owner),
//! this daemon may be configured with several owners in `WAKER_OWNER_PUBKEYS`
//! — each with their own roster, at the same fixed `d` coordinate but a
//! different `authors` entry. One REQ with `authors` set to the whole
//! authorized list covers all of them; [`RosterState`] then tracks each
//! owner's latest roster independently, keyed by owner pubkey, because one
//! owner's roster says nothing about another's membership.
//!
//! # Fail closed on an empty owner list
//!
//! `docs/waker-agent-enrolment.md`'s Two admission modes section (approved,
//! round 2): an empty `WAKER_OWNER_PUBKEYS` means enrolment is **disabled**,
//! not "open" — open mode has no owner-discovery mechanism and is not
//! implemented. [`run_roster_tap`] enforces this itself rather than trusting
//! its caller alone: given no authorized owners, it logs and returns without
//! ever opening a connection. A query with an empty `authors` filter would
//! at best match nothing and at worst behave relay-implementation-defined —
//! refusing before ever constructing one is the same defense-in-depth
//! reasoning the design doc already applies to the `#d` tag re-check below.

use std::collections::HashMap;
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
use crate::enrolment::{RosterBody, SignedRoster};
use crate::feed::reconnect_delay_ms;

/// Subscription id for the daemon's one roster tap. Fixed, like every other
/// tap's own id — a reconnect replaces the old subscription rather than
/// piling up a fresh one.
pub const ROSTER_TAP_SUBSCRIPTION_ID: &str = "buzz-waker-roster";

/// The fixed `d` tag every roster event carries — the public, collision-proof
/// discriminator `docs/waker-agent-enrolment.md`'s Discriminator section
/// settles on. A credential's own `d` is always a canonical 64-hex-char agent
/// pubkey, which can never equal this literal (it contains characters outside
/// `[0-9a-f]`), so the two streams cannot cross by construction — this tap
/// still re-checks it per frame in [`roster_frame`] as defense in depth
/// against a filter bug, the same reasoning already applied to
/// [`crate::bundle_feed`]'s `#p` check.
pub const ROSTER_D_TAG: &str = "waker-enrolment-roster";

/// How long to wait for a frame before treating the tap connection as idle.
///
/// Matches [`crate::bundle_feed::BUNDLE_TAP_IDLE_TIMEOUT_SECS`]'s own
/// reasoning: a roster is republished only on add/remove/rotate, never as a
/// liveness ping, so long quiet stretches are the normal case.
pub const ROSTER_TAP_IDLE_TIMEOUT_SECS: u64 = 300;

/// How many roster events to ask for per owner on subscribe.
///
/// The envelope kind is not parameterized-replaceable (same reasoning as
/// [`crate::bundle_feed::BUNDLE_QUERY_LIMIT`]), so every reissue lands beside
/// its predecessors. Relays return history in `created_at DESC` order, so a
/// small `limit` still reliably returns the newest reissue first regardless
/// of how many older ones exist — this bound only needs to comfortably cover
/// one owner's recent reissue history, not their whole lifetime. Same value
/// as [`crate::bundle_feed::BUNDLE_QUERY_LIMIT`] for the same margin.
pub const ROSTER_QUERY_LIMIT: u32 = 16;

/// The REQ filter for the daemon's roster tap: global, `authors` set to every
/// authorized owner, `#p` pinned to the waker's own identity, `#d` pinned to
/// the fixed roster coordinate.
///
/// `#p` is not optional, mirroring [`crate::bundle_feed::bundle_filter`]'s
/// own doc: the relay refuses an envelope query that omits it.
#[must_use]
pub fn roster_filter(authorized_owners: &[String], waker_pubkey: &str) -> Value {
    let authors: Vec<String> = authorized_owners
        .iter()
        .map(|o| normalize_pubkey(o))
        .collect();
    json!({
        "kinds": [KIND_WAKER_BUNDLE_ENVELOPE],
        "authors": authors,
        "#p": [normalize_pubkey(waker_pubkey)],
        "#d": [ROSTER_D_TAG],
        "limit": ROSTER_QUERY_LIMIT,
    })
}

/// The REQ frame opening the daemon's roster tap.
#[must_use]
pub fn roster_req(authorized_owners: &[String], waker_pubkey: &str) -> Value {
    json!([
        "REQ",
        ROSTER_TAP_SUBSCRIPTION_ID,
        roster_filter(authorized_owners, waker_pubkey)
    ])
}

/// Shared, thread-safe cache of the latest known roster per owner.
///
/// Mirrors [`crate::bundle_feed::BundleState`]'s shape: the tap task
/// ([`run_roster_tap`]) owns the connection and writes here; everything else
/// only reads. Keyed by normalized owner pubkey, since one owner's roster
/// says nothing about another's membership — see the module doc.
#[derive(Debug, Default)]
pub struct RosterState {
    inner: Mutex<HashMap<String, (u64, Arc<RosterBody>)>>,
}

impl RosterState {
    /// A tap with nothing tracked for any owner yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Recover from a poisoned lock rather than propagating it — a panic in
    /// one reader must not permanently blind every future roster lookup.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (u64, Arc<RosterBody>)>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record `body` as the current roster for `owner_pubkey` if its
    /// `roster_version` is newer than whatever is already tracked for that
    /// owner (or nothing is tracked yet). Returns whether it was applied.
    ///
    /// A relay's own `created_at DESC` history ordering usually delivers the
    /// newest reissue first on reconnect, but nothing guarantees that for a
    /// live delivery racing a reconnect's backfill — comparing
    /// `roster_version` rather than trusting delivery order is what makes
    /// this correct either way, the same reasoning
    /// [`crate::floors::FloorStore::admit`] applies to a bundle version.
    fn update_if_newer(&self, owner_pubkey: &str, body: RosterBody) -> bool {
        let owner_pubkey = normalize_pubkey(owner_pubkey);
        let mut map = self.lock();
        let should_apply = match map.get(&owner_pubkey) {
            Some((current_version, _)) => body.roster_version > *current_version,
            None => true,
        };
        if should_apply {
            map.insert(owner_pubkey, (body.roster_version, Arc::new(body)));
        }
        should_apply
    }

    /// The current roster for one owner, if any has been delivered and
    /// tracked on this daemon run yet.
    #[must_use]
    pub fn current(&self, owner_pubkey: &str) -> Option<Arc<RosterBody>> {
        self.lock()
            .get(&normalize_pubkey(owner_pubkey))
            .map(|(_, body)| Arc::clone(body))
    }

    /// Every owner this daemon currently has a tracked roster for.
    #[must_use]
    pub fn known_owners(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }
}

/// What one delivered relay message means for this tap.
///
/// Mirrors [`crate::bundle_feed`]'s own `BundleFrame` convention: everything
/// this tap does not read collapses to [`RosterFrame::Ignored`].
#[derive(Debug, PartialEq, Eq)]
enum RosterFrame {
    /// A verified envelope delivery, authored by one of the authorized
    /// owners, carrying its raw (still-encrypted) content.
    Delivered {
        owner_pubkey: String,
        ciphertext: String,
    },
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

/// Classify one relay message for the roster tap.
///
/// Verification proves only that the stated author signed the event — it
/// does not prove the relay applied this subscription's filter. Re-checking
/// the `#d` tag and the author against `authorized_owners` here is what stops
/// a misrouted or replayed event from ever reaching the decrypt step, same
/// reasoning [`crate::bundle_feed::bundle_frame`] applies for its own tap.
fn roster_frame(authorized_owners: &[String], message: RelayMessage) -> RosterFrame {
    match message {
        RelayMessage::Event {
            subscription_id,
            event,
        } if subscription_id == ROSTER_TAP_SUBSCRIPTION_ID => {
            if let Err(error) = buzz_core::verify_event(&event) {
                return RosterFrame::Rejected {
                    event_id: event.id.to_hex(),
                    reason: error.to_string(),
                };
            }
            if buzz_core::kind::event_kind_u32(&event) != KIND_WAKER_BUNDLE_ENVELOPE {
                return RosterFrame::Ignored;
            }
            if event.tags.identifier() != Some(ROSTER_D_TAG) {
                return RosterFrame::Ignored;
            }
            let signer = normalize_pubkey(&event.pubkey.to_hex());
            if !authorized_owners
                .iter()
                .any(|owner| normalize_pubkey(owner) == signer)
            {
                return RosterFrame::Ignored;
            }
            RosterFrame::Delivered {
                owner_pubkey: signer,
                ciphertext: event.content.clone(),
            }
        }
        RelayMessage::Closed {
            subscription_id,
            message,
        } if subscription_id == ROSTER_TAP_SUBSCRIPTION_ID => RosterFrame::Closed { message },
        _ => RosterFrame::Ignored,
    }
}

/// What a decrypted, verified roster delivery means for the tap's caller.
#[derive(Debug, PartialEq, Eq)]
enum RosterOutcome {
    /// A newer roster than whatever was previously tracked for this owner —
    /// now recorded in [`RosterState`].
    Updated(RosterBody),
    /// A roster whose `roster_version` did not exceed what is already
    /// tracked for this owner — a replay or a reconnect re-delivering
    /// history. Left in place, not an error.
    Stale,
}

/// Decrypt, verify, and track one delivered roster.
///
/// `waker_keys` is the daemon's own identity (the NIP-44 recipient); the
/// ciphertext was encrypted to it. The sender side of the ECDH is
/// `owner_pubkey` — already confirmed to be one of `authorized_owners` by
/// [`roster_frame`]'s pre-filter, though that check is defense in depth: the
/// actual trust decision is [`SignedRoster::verify`], over the *decrypted*
/// body's own signature, independent of which key the outer envelope
/// happened to arrive signed by.
///
/// # Errors
/// A human-readable message on any failure — malformed/oversized ciphertext,
/// a decrypt failure, a parse failure, or a failed inner signature/roster
/// validation. Every path is a refusal to track, never a credential in the
/// error text (a roster carries none, but keeps the same contract as
/// [`crate::bundle_feed::decrypt_verify_and_admit`] for consistency).
fn decrypt_verify_and_track(
    waker_keys: &Keys,
    authorized_owners: &[String],
    owner_pubkey: &str,
    ciphertext: &str,
    state: &RosterState,
) -> Result<RosterOutcome, String> {
    if !NIP44_CONTENT_LEN_RANGE.contains(&ciphertext.len()) {
        return Err(format!(
            "roster ciphertext outside the expected NIP-44 size range ({} chars)",
            ciphertext.len()
        ));
    }
    let owner_pk = nostr::PublicKey::from_hex(owner_pubkey)
        .map_err(|error| format!("malformed owner pubkey: {error}"))?;
    let mut plaintext = nostr::nips::nip44::decrypt(waker_keys.secret_key(), &owner_pk, ciphertext)
        .map_err(|error| format!("NIP-44 decrypt failed: {error}"))?;

    let parsed: Result<SignedRoster, _> = serde_json::from_str(&plaintext);
    plaintext.zeroize();
    let signed = parsed.map_err(|error| format!("malformed roster JSON: {error}"))?;

    let body = signed
        .verify(authorized_owners)
        .map_err(|error| format!("roster verification failed: {error}"))?;

    if state.update_if_newer(owner_pubkey, body.clone()) {
        Ok(RosterOutcome::Updated(body))
    } else {
        Ok(RosterOutcome::Stale)
    }
}

/// Run the daemon's roster tap until `cancel` fires.
///
/// Connects and authenticates as `waker_keys` — **not** any watched agent's
/// identity, see the module doc — subscribes under
/// [`ROSTER_TAP_SUBSCRIPTION_ID`], and folds every delivery into `state` via
/// [`decrypt_verify_and_track`]. Reconnects on any transport error using the
/// same ladder every other tap in this daemon uses ([`reconnect_delay_ms`]).
///
/// Refuses to run at all if `authorized_owners` is empty — see the module
/// doc's Fail closed section. This is a deliberate no-op, not an error: a
/// daemon started with enrolment disabled should log why once and return,
/// not busy-loop reconnecting a query that can never usefully match.
///
/// A malformed, undecryptable, or verification-refused delivery is logged
/// and skipped, not a reconnect — an unauthorized or stale publisher must not
/// be able to knock this tap offline.
pub async fn run_roster_tap(
    relay_url: &str,
    waker_keys: &Keys,
    auth_tag: Option<&Tag>,
    authorized_owners: &[String],
    state: &RosterState,
    cancel: &CancellationToken,
) {
    if authorized_owners.is_empty() {
        tracing::warn!(
            "roster tap has no authorized owners configured (WAKER_OWNER_PUBKEYS is empty); \
             enrolment is disabled — refusing to open a connection"
        );
        return;
    }

    let waker_pubkey = waker_keys.public_key().to_hex();
    let authorized_owners: Vec<String> = authorized_owners
        .iter()
        .map(|owner| normalize_pubkey(owner))
        .collect();
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
                    tracing::warn!(%error, "roster tap connect failed; backing off");
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    continue;
                }
            },
            () = cancel.cancelled() => break,
        };

        if let Err(error) = connection
            .send_raw(&roster_req(&authorized_owners, &waker_pubkey))
            .await
        {
            tracing::warn!(%error, "roster tap subscribe failed; reconnecting");
            consecutive_failures = consecutive_failures.saturating_add(1);
            continue;
        }
        consecutive_failures = 0;

        loop {
            let next = tokio::select! {
                result = connection.next_event(Duration::from_secs(ROSTER_TAP_IDLE_TIMEOUT_SECS)) => result,
                () = cancel.cancelled() => return,
            };

            match next {
                Ok(message) => match roster_frame(&authorized_owners, message) {
                    RosterFrame::Delivered {
                        owner_pubkey,
                        ciphertext,
                    } => {
                        match decrypt_verify_and_track(
                            waker_keys,
                            &authorized_owners,
                            &owner_pubkey,
                            &ciphertext,
                            state,
                        ) {
                            Ok(RosterOutcome::Updated(body)) => {
                                tracing::info!(
                                    owner = %owner_pubkey,
                                    roster_version = body.roster_version,
                                    agent_count = body.entries.len(),
                                    "roster tap tracked a newer roster"
                                );
                            }
                            Ok(RosterOutcome::Stale) => {
                                tracing::info!(
                                    owner = %owner_pubkey,
                                    "roster tap received a roster no newer than the one already tracked; ignoring"
                                );
                            }
                            Err(error) => {
                                tracing::warn!(
                                    owner = %owner_pubkey,
                                    %error,
                                    "roster tap received a delivery it could not track; ignoring"
                                );
                            }
                        }
                    }
                    RosterFrame::Rejected { event_id, reason } => {
                        tracing::warn!(
                            event_id = %event_id,
                            %reason,
                            "roster tap received an event that failed verification; ignoring"
                        );
                    }
                    RosterFrame::Closed { message } => {
                        tracing::warn!(
                            %message,
                            "roster tap subscription closed by relay; reconnecting"
                        );
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        break;
                    }
                    RosterFrame::Ignored => {}
                },
                Err(WsClientError::Timeout) => {
                    // A quiet tap is the normal case — see ROSTER_TAP_IDLE_TIMEOUT_SECS.
                }
                Err(error) => {
                    tracing::warn!(%error, "roster tap connection lost; reconnecting");
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
    use crate::enrolment::RosterEntry;
    use nostr::{EventBuilder, Kind};

    fn roster_event(owner: &Keys, ciphertext: &str) -> nostr::Event {
        EventBuilder::new(Kind::Custom(KIND_WAKER_BUNDLE_ENVELOPE as u16), ciphertext)
            .tags([
                Tag::parse(["d", ROSTER_D_TAG]).unwrap(),
                Tag::parse(["p", &"w".repeat(64)]).unwrap(),
            ])
            .sign_with_keys(owner)
            .expect("sign")
    }

    fn roster_body(entries: Vec<RosterEntry>, roster_version: u64) -> RosterBody {
        RosterBody {
            entries,
            roster_version,
            issued_at: 1_000,
        }
    }

    #[test]
    fn the_query_names_every_authorized_owner_and_the_fixed_roster_coordinate() {
        let owner_a = "a".repeat(64);
        let owner_b = "b".repeat(64);
        let waker_pubkey = "c".repeat(64);
        let filter = roster_filter(&[owner_a.clone(), owner_b.clone()], &waker_pubkey);

        assert_eq!(filter["kinds"], json!([KIND_WAKER_BUNDLE_ENVELOPE]));
        assert_eq!(filter["authors"], json!([owner_a, owner_b]));
        assert_eq!(filter["#p"], json!([waker_pubkey]));
        assert_eq!(filter["#d"], json!([ROSTER_D_TAG]));
        assert!(
            filter["limit"].is_number(),
            "the envelope is not replaceable, so the query must be bounded"
        );
    }

    #[test]
    fn a_verified_delivery_from_an_authorized_owner_is_delivered() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let event = roster_event(&owner, "ciphertext-bytes");

        let frame = roster_frame(
            std::slice::from_ref(&owner_pubkey),
            RelayMessage::Event {
                subscription_id: ROSTER_TAP_SUBSCRIPTION_ID.to_string(),
                event: Box::new(event),
            },
        );
        assert_eq!(
            frame,
            RosterFrame::Delivered {
                owner_pubkey,
                ciphertext: "ciphertext-bytes".to_string()
            }
        );
    }

    #[test]
    fn a_delivery_from_an_unauthorized_signer_is_ignored() {
        let owner = Keys::generate();
        let other_authorized = Keys::generate();
        let event = roster_event(&owner, "ciphertext-bytes");

        let frame = roster_frame(
            &[normalize_pubkey(&other_authorized.public_key().to_hex())],
            RelayMessage::Event {
                subscription_id: ROSTER_TAP_SUBSCRIPTION_ID.to_string(),
                event: Box::new(event),
            },
        );
        assert_eq!(frame, RosterFrame::Ignored);
    }

    /// The credential tap's per-agent `#d` (a canonical 64-hex agent pubkey)
    /// can never collide with the roster's fixed sentinel — this proves the
    /// roster side of that: an event tagged with an agent pubkey instead of
    /// the sentinel must not be mistaken for a roster delivery.
    #[test]
    fn an_event_with_a_non_sentinel_d_tag_is_ignored() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let event = EventBuilder::new(
            Kind::Custom(KIND_WAKER_BUNDLE_ENVELOPE as u16),
            "ciphertext-bytes",
        )
        .tags([
            Tag::parse(["d", &"a".repeat(64)]).unwrap(),
            Tag::parse(["p", &"w".repeat(64)]).unwrap(),
        ])
        .sign_with_keys(&owner)
        .expect("sign");

        let frame = roster_frame(
            &[owner_pubkey],
            RelayMessage::Event {
                subscription_id: ROSTER_TAP_SUBSCRIPTION_ID.to_string(),
                event: Box::new(event),
            },
        );
        assert_eq!(frame, RosterFrame::Ignored);
    }

    #[test]
    fn a_frame_for_another_subscription_is_ignored() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let event = roster_event(&owner, "ciphertext-bytes");

        let frame = roster_frame(
            &[owner_pubkey],
            RelayMessage::Event {
                subscription_id: "some-other-subscription".to_string(),
                event: Box::new(event),
            },
        );
        assert_eq!(frame, RosterFrame::Ignored);
    }

    #[test]
    fn a_closed_frame_for_this_subscription_is_reported() {
        let frame = roster_frame(
            &["a".repeat(64)],
            RelayMessage::Closed {
                subscription_id: ROSTER_TAP_SUBSCRIPTION_ID.to_string(),
                message: "auth-required".to_string(),
            },
        );
        assert_eq!(
            frame,
            RosterFrame::Closed {
                message: "auth-required".to_string()
            }
        );
    }

    #[test]
    fn oversized_ciphertext_is_refused_before_any_decrypt_attempt() {
        let waker = Keys::generate();
        let owner_pubkey = "a".repeat(64);
        let state = RosterState::new();

        let too_long = "x".repeat(NIP44_CONTENT_LEN_RANGE.end() + 1);
        let error = decrypt_verify_and_track(
            &waker,
            std::slice::from_ref(&owner_pubkey),
            &owner_pubkey,
            &too_long,
            &state,
        )
        .unwrap_err();
        assert!(error.contains("size range"), "{error}");
    }

    #[test]
    fn a_valid_round_trip_decrypts_verifies_and_tracks() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let waker = Keys::generate();
        let state = RosterState::new();

        let agent = Keys::generate().public_key().to_hex();
        let body = roster_body(
            vec![RosterEntry {
                agent_pubkey: agent,
                credential_version: 1,
            }],
            1,
        );
        let owner_keypair =
            nostr::secp256k1::Keypair::from_secret_key(nostr::SECP256K1, owner.secret_key());
        let signed = SignedRoster::sign(&body, &owner_keypair).unwrap();
        let plaintext = serde_json::to_string(&signed).unwrap();
        let ciphertext = nostr::nips::nip44::encrypt(
            owner.secret_key(),
            &waker.public_key(),
            &plaintext,
            nostr::nips::nip44::Version::V2,
        )
        .unwrap();

        let outcome = decrypt_verify_and_track(
            &waker,
            std::slice::from_ref(&owner_pubkey),
            &owner_pubkey,
            &ciphertext,
            &state,
        )
        .expect("round trip");
        assert!(matches!(outcome, RosterOutcome::Updated(_)));
        assert_eq!(
            state
                .current(&owner_pubkey)
                .expect("tracked")
                .roster_version,
            1
        );
    }

    /// The headline case [`RosterState::update_if_newer`] exists for: a
    /// reconnect re-delivering an older reissue must not clobber a version
    /// already tracked from a live delivery that arrived first.
    #[test]
    fn a_lower_version_delivered_after_a_higher_one_is_reported_stale_and_does_not_regress() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let waker = Keys::generate();
        let state = RosterState::new();
        let owner_keypair =
            nostr::secp256k1::Keypair::from_secret_key(nostr::SECP256K1, owner.secret_key());

        let encrypt = |body: &RosterBody| -> String {
            let signed = SignedRoster::sign(body, &owner_keypair).unwrap();
            let plaintext = serde_json::to_string(&signed).unwrap();
            nostr::nips::nip44::encrypt(
                owner.secret_key(),
                &waker.public_key(),
                &plaintext,
                nostr::nips::nip44::Version::V2,
            )
            .unwrap()
        };

        let v2 = roster_body(vec![], 2);
        let v1 = roster_body(vec![], 1);

        let outcome = decrypt_verify_and_track(
            &waker,
            std::slice::from_ref(&owner_pubkey),
            &owner_pubkey,
            &encrypt(&v2),
            &state,
        )
        .expect("v2 tracks");
        assert!(matches!(outcome, RosterOutcome::Updated(_)));

        let outcome = decrypt_verify_and_track(
            &waker,
            std::slice::from_ref(&owner_pubkey),
            &owner_pubkey,
            &encrypt(&v1),
            &state,
        )
        .expect("v1 is not an error");
        assert_eq!(outcome, RosterOutcome::Stale);
        assert_eq!(
            state
                .current(&owner_pubkey)
                .expect("tracked")
                .roster_version,
            2,
            "the newer roster must remain tracked"
        );
    }

    #[test]
    fn each_owner_is_tracked_independently() {
        let owner_a = Keys::generate();
        let owner_a_pubkey = normalize_pubkey(&owner_a.public_key().to_hex());
        let owner_b = Keys::generate();
        let owner_b_pubkey = normalize_pubkey(&owner_b.public_key().to_hex());
        let waker = Keys::generate();
        let state = RosterState::new();

        let encrypt_for = |owner: &Keys, body: &RosterBody| -> String {
            let owner_keypair =
                nostr::secp256k1::Keypair::from_secret_key(nostr::SECP256K1, owner.secret_key());
            let signed = SignedRoster::sign(body, &owner_keypair).unwrap();
            let plaintext = serde_json::to_string(&signed).unwrap();
            nostr::nips::nip44::encrypt(
                owner.secret_key(),
                &waker.public_key(),
                &plaintext,
                nostr::nips::nip44::Version::V2,
            )
            .unwrap()
        };

        let authorized = vec![owner_a_pubkey.clone(), owner_b_pubkey.clone()];
        decrypt_verify_and_track(
            &waker,
            &authorized,
            &owner_a_pubkey,
            &encrypt_for(&owner_a, &roster_body(vec![], 5)),
            &state,
        )
        .expect("owner a tracks");
        decrypt_verify_and_track(
            &waker,
            &authorized,
            &owner_b_pubkey,
            &encrypt_for(&owner_b, &roster_body(vec![], 1)),
            &state,
        )
        .expect("owner b tracks");

        assert_eq!(state.current(&owner_a_pubkey).unwrap().roster_version, 5);
        assert_eq!(state.current(&owner_b_pubkey).unwrap().roster_version, 1);
        let mut owners = state.known_owners();
        owners.sort();
        let mut expected = vec![owner_a_pubkey, owner_b_pubkey];
        expected.sort();
        assert_eq!(owners, expected);
    }

    #[test]
    fn a_roster_signed_by_a_never_authorized_owner_is_refused_at_verify() {
        // Bypasses the frame pre-filter to prove the actual trust decision
        // (SignedRoster::verify) independently refuses an unauthorized
        // signer too, not just the pre-filter.
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let waker = Keys::generate();
        let state = RosterState::new();
        let owner_keypair =
            nostr::secp256k1::Keypair::from_secret_key(nostr::SECP256K1, owner.secret_key());
        let signed = SignedRoster::sign(&roster_body(vec![], 1), &owner_keypair).unwrap();
        let plaintext = serde_json::to_string(&signed).unwrap();
        let ciphertext = nostr::nips::nip44::encrypt(
            owner.secret_key(),
            &waker.public_key(),
            &plaintext,
            nostr::nips::nip44::Version::V2,
        )
        .unwrap();

        let error = decrypt_verify_and_track(
            &waker,
            &["z".repeat(64)],
            &owner_pubkey,
            &ciphertext,
            &state,
        )
        .expect_err("unauthorized owner refused");
        assert!(error.contains("verification failed"), "{error}");
    }
}
