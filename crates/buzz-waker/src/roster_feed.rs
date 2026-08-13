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
//! # Multiple owners, several queries
//!
//! Unlike a bundle or credential tap (pinned to one already-known owner),
//! this daemon may be configured with several owners in `WAKER_OWNER_PUBKEYS`
//! — each with their own roster, at the same fixed `d` coordinate but a
//! different `authors` entry. Each owner gets its own filter — see
//! [`roster_filters`]'s own doc for why one shared filter would apply the
//! query's `limit` across every owner combined rather than to each of them —
//! and [`roster_reqs`] batches those filters into REQ frames of at most
//! [`ROSTER_MAX_FILTERS_PER_REQ`], the relay's own per-REQ cap, opening
//! however many subscriptions that takes rather than ever emitting a REQ
//! the relay would refuse outright. [`RosterState`] then tracks each
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
//!
//! # Durable per-owner version floor
//!
//! [`RosterState`] alone only tracks the highest `roster_version` seen this
//! process lifetime. Because the envelope kind is not replaceable, an owner's
//! old reissues stay queryable forever, so a relay can replay one after a
//! restart and this tap would accept it — resurrecting an agent a newer
//! roster already removed. [`run_roster_tap`] closes that hole the same way
//! [`crate::floors::FloorStore`] already closes it for bundle versions
//! (**G2**): one [`FloorStore`] per owner, opened lazily under
//! `roster_floor_dir` and re-read from disk on every delivery, so the floor
//! survives a restart even though [`RosterState`] itself does not.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::Path;
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
use crate::floors::{FloorError, FloorStore};

/// Base subscription id for the daemon's roster tap. Fixed, like every other
/// tap's own id — a reconnect replaces the old subscriptions rather than
/// piling up fresh ones. Never used bare as a wire subscription id itself:
/// every actual REQ carries [`roster_subscription_id`]'s batch-suffixed
/// form, even when there is only one batch — see that function's own doc.
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

/// One REQ filter per authorized owner: same `#p`/`#d` pinning as before, but
/// `authors` narrowed to a single owner so each filter gets its own
/// [`ROSTER_QUERY_LIMIT`].
///
/// A single filter with every owner in `authors` would apply the limit once
/// across *all* of them combined — `buzz-relay`'s `handle_req` runs a
/// multi-filter REQ as one independent, independently-limited DB query per
/// filter (NIP-01 OR semantics), so a burst of reissues from one owner can
/// only ever crowd out that owner's own history, never another authorized
/// owner's latest roster.
///
/// `#p` is not optional, mirroring [`crate::bundle_feed::bundle_filter`]'s
/// own doc: the relay refuses an envelope query that omits it.
#[must_use]
pub fn roster_filters(authorized_owners: &[String], waker_pubkey: &str) -> Vec<Value> {
    let waker_pubkey = normalize_pubkey(waker_pubkey);
    authorized_owners
        .iter()
        .map(|owner| {
            json!({
                "kinds": [KIND_WAKER_BUNDLE_ENVELOPE],
                "authors": [normalize_pubkey(owner)],
                "#p": [waker_pubkey],
                "#d": [ROSTER_D_TAG],
                "limit": ROSTER_QUERY_LIMIT,
            })
        })
        .collect()
}

/// The relay's own per-REQ filter cap: `buzz-relay/src/protocol.rs`'s
/// `MAX_FILTERS_PER_REQ`, advertised over NIP-11 as `max_filters: 10`
/// (`crates/buzz-relay/src/nip11.rs`). A REQ over this limit is refused
/// outright, not truncated — [`roster_reqs`] batches `authorized_owners`
/// at this bound instead of ever emitting one. Not learned from the
/// relay at runtime: this tap connects before any HTTP/NIP-11 client
/// exists in this daemon, and the owner list is deploy-time config, not
/// something worth an extra round trip to discover. If the relay's own
/// limit ever changes, this constant has to change with it.
pub const ROSTER_MAX_FILTERS_PER_REQ: usize = 10;

/// The subscription id for roster batch `index` — every batch gets its own
/// id so a relay `CLOSED` or delivery can be attributed to the batch it
/// belongs to. Always suffixed, even for the common case of one batch,
/// rather than special-casing batch 0 onto the bare
/// [`ROSTER_TAP_SUBSCRIPTION_ID`] — one shape for [`roster_frame`] to
/// match against instead of two.
#[must_use]
pub fn roster_subscription_id(index: usize) -> String {
    format!("{ROSTER_TAP_SUBSCRIPTION_ID}-{index}")
}

/// The REQ frames opening the daemon's roster tap: `authorized_owners`
/// split into batches of at most [`ROSTER_MAX_FILTERS_PER_REQ`], one REQ
/// frame per batch under its own [`roster_subscription_id`].
///
/// A daemon with more authorized owners than one REQ can hold still needs
/// every owner's roster, not just the first ten — multiple subscriptions
/// on the same connection is how NIP-01 already supports "more filters
/// than one REQ can carry" (the same reason a client opens several
/// subscriptions rather than one giant one). Splitting here, rather than
/// silently dropping owners past the cap or refusing to start, is what
/// makes an 11-owner configuration behave the same as a 10-owner one.
#[must_use]
pub fn roster_reqs(authorized_owners: &[String], waker_pubkey: &str) -> Vec<(String, Value)> {
    authorized_owners
        .chunks(ROSTER_MAX_FILTERS_PER_REQ)
        .enumerate()
        .map(|(index, owners)| {
            let subscription_id = roster_subscription_id(index);
            let mut frame = vec![
                Value::String("REQ".to_string()),
                Value::String(subscription_id.clone()),
            ];
            frame.extend(roster_filters(owners, waker_pubkey));
            (subscription_id, Value::Array(frame))
        })
        .collect()
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
/// `subscription_ids` is every batch id this run of the tap opened (see
/// [`roster_reqs`]) — more than one once `authorized_owners` exceeds
/// [`ROSTER_MAX_FILTERS_PER_REQ`], so a single fixed id can no longer be
/// the test.
///
/// Verification proves only that the stated author signed the event — it
/// does not prove the relay applied this subscription's filter. Re-checking
/// the `#d` tag and the author against `authorized_owners` here is what stops
/// a misrouted or replayed event from ever reaching the decrypt step, same
/// reasoning [`crate::bundle_feed::bundle_frame`] applies for its own tap.
fn roster_frame(
    authorized_owners: &[String],
    subscription_ids: &[String],
    message: RelayMessage,
) -> RosterFrame {
    match message {
        RelayMessage::Event {
            subscription_id,
            event,
        } if subscription_ids.contains(&subscription_id) => {
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
        } if subscription_ids.contains(&subscription_id) => RosterFrame::Closed { message },
        _ => RosterFrame::Ignored,
    }
}

/// What a decrypted, verified roster delivery means for the tap's caller.
#[derive(Debug, PartialEq, Eq)]
enum RosterOutcome {
    /// A newer roster than whatever was previously tracked for this owner —
    /// now recorded in [`RosterState`] and durably admitted by this owner's
    /// [`FloorStore`].
    Updated(RosterBody),
    /// A roster whose `roster_version` did not exceed what is already
    /// tracked for this owner — either this process's own [`RosterState`]
    /// or, after a restart, the durable per-owner [`FloorStore`] floor. A
    /// replay or a reconnect re-delivering history. Left in place, not an
    /// error.
    Stale,
}

/// Decrypt, verify, durably admit, and track one delivered roster.
///
/// `waker_keys` is the daemon's own identity (the NIP-44 recipient); the
/// ciphertext was encrypted to it. The sender side of the ECDH is
/// `owner_pubkey` — already confirmed to be one of `authorized_owners` by
/// [`roster_frame`]'s pre-filter, though that check is defense in depth: the
/// actual trust decision is [`SignedRoster::verify`], over the *decrypted*
/// body's own signature, independent of which key the outer envelope
/// happened to arrive signed by.
///
/// `floor_store` is `owner_pubkey`'s own durable version floor (see the
/// module doc's Durable per-owner version floor section) — checked and
/// advanced, under its own fence, before the delivery ever reaches
/// [`RosterState`]. This is what makes the anti-replay guarantee survive a
/// restart; [`RosterState`] alone only remembers for this process's
/// lifetime.
///
/// # Errors
/// A human-readable message on any failure — malformed/oversized ciphertext,
/// a decrypt failure, a parse failure, a failed inner signature/roster
/// validation, or a durable floor that could not be persisted. Every path is
/// a refusal to track, never a credential in the error text (a roster
/// carries none, but keeps the same contract as
/// [`crate::bundle_feed::decrypt_verify_and_admit`] for consistency).
fn decrypt_verify_and_track(
    waker_keys: &Keys,
    authorized_owners: &[String],
    owner_pubkey: &str,
    ciphertext: &str,
    floor_store: &mut FloorStore,
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

    // Durable floor first: it is the only part of this that survives a
    // restart, so it must gate `RosterState` rather than the other way
    // round. `RolledBack` is exactly the replay-after-restart case this
    // floor exists for — a stale delivery, not an error.
    match floor_store.admit(body.roster_version) {
        Ok(()) => {}
        Err(FloorError::RolledBack { .. }) => return Ok(RosterOutcome::Stale),
        Err(error) => {
            return Err(format!("roster floor could not be advanced: {error}"));
        }
    }

    if state.update_if_newer(owner_pubkey, body.clone()) {
        Ok(RosterOutcome::Updated(body))
    } else {
        Ok(RosterOutcome::Stale)
    }
}

/// Open `owner_pubkey`'s durable roster floor under `dir`, creating it at
/// version 0 the first time this daemon ever sees that owner.
///
/// Unlike [`FloorStore::enroll`]'s use for bundle/credential state, a
/// missing file here is the ordinary cold-start case (this daemon has never
/// tracked a roster from this owner before) rather than a suspicious gap —
/// the owner's authenticity already comes from `WAKER_OWNER_PUBKEYS` and
/// [`SignedRoster::verify`], not from anything pinned in this file. Once
/// created, the file is fenced and read-before-decide exactly like every
/// other [`FloorStore`], so a version this daemon has already admitted
/// cannot be forgotten by a later restart.
fn open_or_create_owner_floor(dir: &Path, owner_pubkey: &str) -> Result<FloorStore, FloorError> {
    let path = dir.join(format!("{owner_pubkey}.json"));
    match FloorStore::open(&path) {
        Ok(store) => Ok(store),
        Err(FloorError::NotEnrolled { .. }) => FloorStore::enroll(&path, owner_pubkey),
        Err(error) => Err(error),
    }
}

/// Run the daemon's roster tap until `cancel` fires.
///
/// Connects and authenticates as `waker_keys` — **not** any watched agent's
/// identity, see the module doc — opens one subscription per
/// [`roster_reqs`] batch, and folds every delivery into `state` via
/// [`decrypt_verify_and_track`]. Reconnects on any transport error using the
/// same ladder every other tap in this daemon uses ([`reconnect_delay_ms`]);
/// a failure partway through subscribing (e.g. batch 2 of 3) reconnects the
/// whole connection rather than leaving a partial subscription set running,
/// same as any other subscribe failure.
///
/// Refuses to run at all if `authorized_owners` is empty — see the module
/// doc's Fail closed section. This is a deliberate no-op, not an error: a
/// daemon started with enrolment disabled should log why once and return,
/// not busy-loop reconnecting a query that can never usefully match.
///
/// `roster_floor_dir` holds each authorized owner's durable version floor
/// (one file per owner, opened lazily — see the module doc's Durable
/// per-owner version floor section). Owned by this task for its lifetime,
/// the same single-writer shape [`crate::bundle_feed::run_bundle_tap`] uses
/// for its own `floor_store`.
///
/// A malformed, undecryptable, or verification-refused delivery is logged
/// and skipped, not a reconnect — an unauthorized or stale publisher must not
/// be able to knock this tap offline. The same is true of an owner whose
/// durable floor this daemon cannot open or create (e.g. a permissions
/// problem under `roster_floor_dir`): that owner's deliveries are skipped
/// and logged, not treated as a reason to drop the whole connection.
#[allow(clippy::too_many_arguments)]
pub async fn run_roster_tap(
    relay_url: &str,
    waker_keys: &Keys,
    auth_tag: Option<&Tag>,
    authorized_owners: &[String],
    roster_floor_dir: &Path,
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

    if let Err(error) = std::fs::create_dir_all(roster_floor_dir) {
        tracing::error!(
            dir = %roster_floor_dir.display(),
            %error,
            "roster tap could not create its durable floor directory; refusing to run"
        );
        return;
    }

    let waker_pubkey = waker_keys.public_key().to_hex();
    let authorized_owners: Vec<String> = authorized_owners
        .iter()
        .map(|owner| normalize_pubkey(owner))
        .collect();
    let mut floors: HashMap<String, FloorStore> = HashMap::new();
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

        let reqs = roster_reqs(&authorized_owners, &waker_pubkey);
        let subscription_ids: Vec<String> = reqs.iter().map(|(id, _)| id.clone()).collect();
        let mut subscribed = true;
        for (_, req) in &reqs {
            if let Err(error) = connection.send_raw(req).await {
                tracing::warn!(%error, "roster tap subscribe failed; reconnecting");
                consecutive_failures = consecutive_failures.saturating_add(1);
                subscribed = false;
                break;
            }
        }
        if !subscribed {
            continue;
        }
        consecutive_failures = 0;

        loop {
            let next = tokio::select! {
                result = connection.next_event(Duration::from_secs(ROSTER_TAP_IDLE_TIMEOUT_SECS)) => result,
                () = cancel.cancelled() => return,
            };

            match next {
                Ok(message) => match roster_frame(&authorized_owners, &subscription_ids, message) {
                    RosterFrame::Delivered {
                        owner_pubkey,
                        ciphertext,
                    } => {
                        let floor_store = match floors.entry(owner_pubkey.clone()) {
                            Entry::Occupied(entry) => entry.into_mut(),
                            Entry::Vacant(entry) => {
                                match open_or_create_owner_floor(roster_floor_dir, &owner_pubkey) {
                                    Ok(store) => entry.insert(store),
                                    Err(error) => {
                                        tracing::warn!(
                                            owner = %owner_pubkey,
                                            %error,
                                            "roster tap could not open this owner's durable floor; skipping delivery"
                                        );
                                        continue;
                                    }
                                }
                            }
                        };
                        match decrypt_verify_and_track(
                            waker_keys,
                            &authorized_owners,
                            &owner_pubkey,
                            &ciphertext,
                            floor_store,
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

    /// A fresh durable floor for one owner, backed by its own tempdir file —
    /// mirrors [`open_or_create_owner_floor`] but lets a test hold the
    /// `TempDir` so a later reopen in the same test simulates a restart.
    fn test_floor(dir: &tempfile::TempDir, owner_pubkey: &str) -> FloorStore {
        open_or_create_owner_floor(dir.path(), owner_pubkey).expect("open or create floor")
    }

    #[test]
    fn each_filter_names_one_owner_with_its_own_bounded_limit() {
        let owner_a = "a".repeat(64);
        let owner_b = "b".repeat(64);
        let waker_pubkey = "c".repeat(64);
        let filters = roster_filters(&[owner_a.clone(), owner_b.clone()], &waker_pubkey);

        assert_eq!(filters.len(), 2, "one filter per authorized owner");
        for (filter, owner) in filters.iter().zip([&owner_a, &owner_b]) {
            assert_eq!(filter["kinds"], json!([KIND_WAKER_BUNDLE_ENVELOPE]));
            assert_eq!(
                filter["authors"],
                json!([owner]),
                "each filter's authors must be exactly its own owner, not every owner"
            );
            assert_eq!(filter["#p"], json!([waker_pubkey]));
            assert_eq!(filter["#d"], json!([ROSTER_D_TAG]));
            assert!(
                filter["limit"].is_number(),
                "the envelope is not replaceable, so every filter must be bounded"
            );
        }
    }

    #[test]
    fn one_owner_still_produces_exactly_one_req_under_a_suffixed_subscription_id() {
        let owner_a = "a".repeat(64);
        let owner_b = "b".repeat(64);
        let waker_pubkey = "c".repeat(64);
        let reqs = roster_reqs(&[owner_a, owner_b], &waker_pubkey);

        assert_eq!(reqs.len(), 1, "two owners fit in one batch");
        let (subscription_id, req) = &reqs[0];
        assert_eq!(subscription_id, &roster_subscription_id(0));
        let frame = req.as_array().expect("REQ is an array");

        assert_eq!(frame[0], json!("REQ"));
        assert_eq!(frame[1], json!(subscription_id));
        assert_eq!(
            frame.len(),
            4,
            "\"REQ\", subscription id, then one filter per owner"
        );
    }

    #[test]
    fn owners_past_the_relay_filter_cap_split_into_a_second_req() {
        let waker_pubkey = "c".repeat(64);
        let owners: Vec<String> = (0..(ROSTER_MAX_FILTERS_PER_REQ + 1))
            .map(|i| format!("{i:064x}"))
            .collect();

        let reqs = roster_reqs(&owners, &waker_pubkey);

        assert_eq!(
            reqs.len(),
            2,
            "one owner over the cap must open a second subscription, not be dropped or refuse to start"
        );
        let (first_id, first_req) = &reqs[0];
        let (second_id, second_req) = &reqs[1];
        assert_eq!(first_id, &roster_subscription_id(0));
        assert_eq!(second_id, &roster_subscription_id(1));
        assert_ne!(first_id, second_id);

        let first_filters = first_req.as_array().expect("array").len() - 2;
        let second_filters = second_req.as_array().expect("array").len() - 2;
        assert_eq!(first_filters, ROSTER_MAX_FILTERS_PER_REQ);
        assert_eq!(second_filters, 1);
        assert_eq!(
            first_filters + second_filters,
            owners.len(),
            "every owner must appear in exactly one batch"
        );
    }

    #[test]
    fn a_verified_delivery_from_an_authorized_owner_is_delivered() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let event = roster_event(&owner, "ciphertext-bytes");
        let subscription_id = roster_subscription_id(0);

        let frame = roster_frame(
            std::slice::from_ref(&owner_pubkey),
            std::slice::from_ref(&subscription_id),
            RelayMessage::Event {
                subscription_id: subscription_id.clone(),
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

    /// A daemon with owners split across two batches must still recognize a
    /// delivery on the second batch's subscription id, not just the first.
    #[test]
    fn a_delivery_on_a_later_batchs_subscription_is_still_delivered() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let event = roster_event(&owner, "ciphertext-bytes");
        let subscription_ids = vec![roster_subscription_id(0), roster_subscription_id(1)];

        let frame = roster_frame(
            std::slice::from_ref(&owner_pubkey),
            &subscription_ids,
            RelayMessage::Event {
                subscription_id: roster_subscription_id(1),
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
        let subscription_id = roster_subscription_id(0);

        let frame = roster_frame(
            &[normalize_pubkey(&other_authorized.public_key().to_hex())],
            std::slice::from_ref(&subscription_id),
            RelayMessage::Event {
                subscription_id: subscription_id.clone(),
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
        let subscription_id = roster_subscription_id(0);

        let frame = roster_frame(
            &[owner_pubkey],
            std::slice::from_ref(&subscription_id),
            RelayMessage::Event {
                subscription_id: subscription_id.clone(),
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
            std::slice::from_ref(&roster_subscription_id(0)),
            RelayMessage::Event {
                subscription_id: "some-other-subscription".to_string(),
                event: Box::new(event),
            },
        );
        assert_eq!(frame, RosterFrame::Ignored);
    }

    #[test]
    fn a_closed_frame_for_this_subscription_is_reported() {
        let subscription_id = roster_subscription_id(0);
        let frame = roster_frame(
            &["a".repeat(64)],
            std::slice::from_ref(&subscription_id),
            RelayMessage::Closed {
                subscription_id: subscription_id.clone(),
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
        let dir = tempfile::tempdir().expect("tempdir");
        let mut floor = test_floor(&dir, &owner_pubkey);

        let too_long = "x".repeat(NIP44_CONTENT_LEN_RANGE.end() + 1);
        let error = decrypt_verify_and_track(
            &waker,
            std::slice::from_ref(&owner_pubkey),
            &owner_pubkey,
            &too_long,
            &mut floor,
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
        let dir = tempfile::tempdir().expect("tempdir");
        let mut floor = test_floor(&dir, &owner_pubkey);

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
            &mut floor,
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
        let dir = tempfile::tempdir().expect("tempdir");
        let mut floor = test_floor(&dir, &owner_pubkey);
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
            &mut floor,
            &state,
        )
        .expect("v2 tracks");
        assert!(matches!(outcome, RosterOutcome::Updated(_)));

        let outcome = decrypt_verify_and_track(
            &waker,
            std::slice::from_ref(&owner_pubkey),
            &owner_pubkey,
            &encrypt(&v1),
            &mut floor,
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

    /// The P1 fix's headline case: [`RosterState`] alone forgets on restart,
    /// but the durable per-owner floor must not. Simulates a restart by
    /// dropping the in-memory `RosterState` and reopening the on-disk
    /// [`FloorStore`] from the same path, then replays the old, still
    /// validly-signed v1 roster a relay could still serve from history.
    #[test]
    fn a_replayed_older_roster_is_refused_after_a_simulated_restart() {
        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let waker = Keys::generate();
        let dir = tempfile::tempdir().expect("tempdir");
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

        let v9 = roster_body(vec![], 9);
        let v4 = roster_body(vec![], 4);

        {
            let state = RosterState::new();
            let mut floor = test_floor(&dir, &owner_pubkey);
            let outcome = decrypt_verify_and_track(
                &waker,
                std::slice::from_ref(&owner_pubkey),
                &owner_pubkey,
                &encrypt(&v9),
                &mut floor,
                &state,
            )
            .expect("v9 tracks");
            assert!(matches!(outcome, RosterOutcome::Updated(_)));
        }

        // Simulated restart: fresh RosterState (nothing tracked this
        // process lifetime), floor reopened from disk.
        let state = RosterState::new();
        let mut floor = test_floor(&dir, &owner_pubkey);
        assert_eq!(
            floor.snapshot().highest_accepted_version,
            9,
            "the durable floor must have survived the simulated restart"
        );

        let outcome = decrypt_verify_and_track(
            &waker,
            std::slice::from_ref(&owner_pubkey),
            &owner_pubkey,
            &encrypt(&v4),
            &mut floor,
            &state,
        )
        .expect("a replayed older roster is refused, not an error");
        assert_eq!(
            outcome,
            RosterOutcome::Stale,
            "the durable floor must refuse the replay even though RosterState forgot it"
        );
        assert!(
            state.current(&owner_pubkey).is_none(),
            "a refused delivery must never reach RosterState"
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
        let dir = tempfile::tempdir().expect("tempdir");
        let mut floor_a = test_floor(&dir, &owner_a_pubkey);
        let mut floor_b = test_floor(&dir, &owner_b_pubkey);

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
            &mut floor_a,
            &state,
        )
        .expect("owner a tracks");
        decrypt_verify_and_track(
            &waker,
            &authorized,
            &owner_b_pubkey,
            &encrypt_for(&owner_b, &roster_body(vec![], 1)),
            &mut floor_b,
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
        let dir = tempfile::tempdir().expect("tempdir");
        let mut floor = test_floor(&dir, &owner_pubkey);
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
            &mut floor,
            &state,
        )
        .expect_err("unauthorized owner refused");
        assert!(error.contains("verification failed"), "{error}");
    }
}
