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
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroize;

use crate::bundle::{LaunchBundleBody, SignedLaunchBundle};
use crate::decide::{normalize_pubkey, TriggerEvent};
use crate::feed::reconnect_delay_ms;
use crate::floors::FloorStore;
use crate::start_request::{
    admit_start_request, start_request_envelope_keys, start_request_frame, start_request_req,
    StartRequestFrame,
};

/// Subscription id for one agent's bundle-delivery tap. Fixed, like the
/// mention feed's and the presence tap's own ids — a reconnect replaces the
/// old subscription rather than piling up a fresh one.
pub const BUNDLE_TAP_SUBSCRIPTION_ID: &str = "buzz-waker-bundle";

/// Subscription id for the tap's bundle re-query, the fence a start request
/// waits behind ([`run_bundle_tap`]). At most one is in flight per
/// connection, so its EOSE is never ambiguous.
pub const BUNDLE_SYNC_SUBSCRIPTION_ID: &str = "buzz-waker-bundle-sync";

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
///
/// `pub(crate)` so [`crate::roster_feed`] and [`crate::credential_feed`] (the
/// enrolment taps, which decrypt the same kind-1059 envelope) apply the exact
/// same bound rather than a second copy of this magic range.
pub(crate) const NIP44_CONTENT_LEN_RANGE: std::ops::RangeInclusive<usize> = 132..=87472;

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

/// The REQ frame re-querying the bundle history under
/// [`BUNDLE_SYNC_SUBSCRIPTION_ID`] — same filter as [`bundle_req`].
fn bundle_sync_req(owner_pubkey: &str, agent_pubkey: &str) -> Value {
    json!([
        "REQ",
        BUNDLE_SYNC_SUBSCRIPTION_ID,
        bundle_filter(owner_pubkey, agent_pubkey)
    ])
}

fn is_bundle_subscription(subscription_id: &str) -> bool {
    subscription_id == BUNDLE_TAP_SUBSCRIPTION_ID || subscription_id == BUNDLE_SYNC_SUBSCRIPTION_ID
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
    /// The bundle re-query has delivered all of its history.
    SyncComplete,
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
        } if is_bundle_subscription(&subscription_id) => {
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
        } if is_bundle_subscription(&subscription_id) => BundleFrame::Closed { message },
        RelayMessage::Eose { subscription_id }
            if subscription_id == BUNDLE_SYNC_SUBSCRIPTION_ID =>
        {
            BundleFrame::SyncComplete
        }
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
    /// An owner-signed revocation whose version is below the version already
    /// admitted — a later, still-valid reissue has already superseded it.
    /// The floor was still raised (defense in depth for any *future*
    /// delivery below it), but the currently held bundle must not be
    /// cleared: the relay has no delivery order guarantee, so this delivery
    /// can reach the tap after the reissue that supersedes it, on a
    /// reconnect that replays history in `created_at DESC` order.
    StaleRevocation,
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

        // The floor was just re-read (and possibly raised) under the fence,
        // so this reflects the freshest known `highest_accepted_version` —
        // including one admitted by a delivery this same connection already
        // processed. A revocation below it is stale: a later reissue already
        // superseded it, and clearing the cache here would throw away that
        // still-valid, still-current bundle.
        if body.bundle_version < floor_store.snapshot().highest_accepted_version {
            return Ok(BundleOutcome::StaleRevocation);
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
///
/// The same connection carries this agent's start requests on a second
/// subscription ([`crate::start_request`]): the same authentication, the same
/// envelope kind, so no extra socket per agent. An admitted request goes to
/// the wake loop over `start_requests`; the loop is what claims and acts on it.
/// A request waits in [`PendingStartRequests`] until that channel has room,
/// and the wait is one arm of the same `select!` that reads the socket, so a
/// wake loop that is not taking requests (its mention feed is reconnecting)
/// never stops this tap reading bundles. That ordering is load-bearing: a
/// revocation read behind a stalled handoff would reach [`BundleState`] only
/// after the wake loop had taken the request and snapshotted the revoked
/// bundle.
///
/// A request is also held until a bundle re-query ([`BundleSync`]) issued
/// after the request was read has reached EOSE. Sharing a socket does not
/// order the two subscriptions: the relay runs each REQ's history and each
/// event's live fan-out in its own task, so a revocation the relay stored
/// before a request can still reach this tap after it. A query issued after
/// the request arrived is not subject to that: its history includes every
/// revocation the relay had stored by then, and its EOSE comes last.
#[allow(clippy::too_many_arguments)]
pub async fn run_bundle_tap(
    relay_url: &str,
    keys: &Keys,
    auth_tag: Option<&Tag>,
    owner_pubkey: &str,
    floor_store: &mut FloorStore,
    state: &BundleState,
    start_requests: &mpsc::Sender<TriggerEvent>,
    cancel: &CancellationToken,
) {
    let agent_pubkey = keys.public_key().to_hex();
    let owner_pubkey = normalize_pubkey(owner_pubkey);
    // Without a valid owner key no request could verify either, so the
    // bundle half runs alone rather than the whole tap refusing to start.
    let envelope_author = match nostr::PublicKey::from_hex(&owner_pubkey)
        .map_err(|e| e.to_string())
        .and_then(|owner| {
            start_request_envelope_keys(keys.secret_key(), &owner).map_err(|e| e.to_string())
        }) {
        Ok(envelope) => Some(envelope.public_key()),
        Err(error) => {
            tracing::error!(
                agent = %agent_pubkey,
                %error,
                "bundle tap cannot derive the start-request envelope key; start requests disabled"
            );
            None
        }
    };
    let mut consecutive_failures = 0u32;
    // Outlives a reconnect: a request read before the socket dropped is still
    // the owner's.
    let mut pending = PendingStartRequests::default();

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

        // Per connection: a re-query sent on a dropped socket answers nothing,
        // so every request still held waits for one on this socket.
        let mut sync = BundleSync::default();
        pending.require_sync(1);
        if let Err(error) = connection
            .send_raw(&bundle_req(&owner_pubkey, &agent_pubkey))
            .await
        {
            tracing::warn!(agent = %agent_pubkey, %error, "bundle tap subscribe failed; reconnecting");
            consecutive_failures = consecutive_failures.saturating_add(1);
            continue;
        }
        if let Some(author) = &envelope_author {
            if let Err(error) = connection
                .send_raw(&start_request_req(&agent_pubkey, author, now_secs()))
                .await
            {
                tracing::warn!(agent = %agent_pubkey, %error, "start request subscribe failed; reconnecting");
                consecutive_failures = consecutive_failures.saturating_add(1);
                continue;
            }
        }
        consecutive_failures = 0;

        loop {
            if pending.needs_sync(&sync) {
                if let Err(error) = connection
                    .send_raw(&bundle_sync_req(&owner_pubkey, &agent_pubkey))
                    .await
                {
                    tracing::warn!(agent = %agent_pubkey, %error, "bundle re-query failed; reconnecting");
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    break;
                }
                sync.issued += 1;
                sync.in_flight = true;
            }

            // `biased` puts any frame the runtime has already reported ahead
            // of a handoff. `next_event` is cancel-safe (it only awaits the
            // stream's `next`), so losing to a permit drops no frame.
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                result = connection.next_event(Duration::from_secs(BUNDLE_TAP_IDLE_TIMEOUT_SECS)) => result,
                permit = start_requests.reserve(), if pending.ready(&sync) => {
                    pending.hand_over(permit, &agent_pubkey);
                    continue;
                }
            };

            let message = match next {
                Ok(message) => match start_request_frame(&message) {
                    StartRequestFrame::Event(event) => {
                        if let Some(author) = &envelope_author {
                            if let Some(trigger) = verify_start_request(
                                keys,
                                &owner_pubkey,
                                author,
                                event,
                                &agent_pubkey,
                            ) {
                                pending.push(trigger, &sync, &agent_pubkey);
                            }
                        }
                        continue;
                    }
                    StartRequestFrame::Closed(reason) => {
                        tracing::warn!(
                            agent = %agent_pubkey,
                            %reason,
                            "start request subscription closed by relay; reconnecting"
                        );
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        break;
                    }
                    StartRequestFrame::NotOurs => Ok(message),
                },
                Err(error) => Err(error),
            };

            match message {
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
                            Ok(BundleOutcome::StaleRevocation) => {
                                tracing::info!(
                                    agent = %agent_pubkey,
                                    "bundle tap received a revocation already superseded by a newer admitted bundle; leaving the cache in place"
                                );
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
                    BundleFrame::SyncComplete => {
                        sync.completed = sync.issued;
                        sync.in_flight = false;
                        if let Err(error) = connection
                            .send_raw(&json!(["CLOSE", BUNDLE_SYNC_SUBSCRIPTION_ID]))
                            .await
                        {
                            tracing::warn!(agent = %agent_pubkey, %error, "bundle re-query close failed; reconnecting");
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            break;
                        }
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

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// How many verified start requests the tap holds while the wake loop is not
/// taking them — the recovery query's own window, so one full replay fits.
const START_REQUEST_BACKLOG: usize = 64;

/// Verified start requests read off the socket and not yet taken by the wake
/// loop.
///
/// Bounded, deduplicated by event id, and full means dropping the OLDEST.
/// Every request asks for the same thing — start this agent — so a newer one
/// does whatever an older one would; and only the owner/agent pair can author
/// one ([`start_request_envelope_keys`] pins the envelope signer, the inner
/// signature pins the owner), so filling this takes the owner's own presses.
/// Dropping the newest instead is the round-1 failure: a backlog of replays
/// the cursor already holds crowding out the press that matters.
///
/// Each request carries the number of the bundle re-query it waits for on
/// this connection ([`BundleSync`]); the queue is in read order, so those
/// numbers never decrease front to back.
#[derive(Debug, Default)]
struct PendingStartRequests {
    queue: std::collections::VecDeque<(TriggerEvent, u64)>,
}

/// The bundle re-queries issued on one connection, counted from 1.
#[derive(Debug, Default)]
struct BundleSync {
    issued: u64,
    completed: u64,
    in_flight: bool,
}

impl PendingStartRequests {
    /// Whether a re-query must be sent now: a held request waits for one not
    /// yet issued, and none is in flight (one at a time keeps EOSE
    /// unambiguous; a request read meanwhile waits for the next).
    fn needs_sync(&self, sync: &BundleSync) -> bool {
        !sync.in_flight
            && self
                .queue
                .back()
                .is_some_and(|(_, needs)| *needs > sync.issued)
    }

    /// Whether the oldest request's re-query has completed.
    fn ready(&self, sync: &BundleSync) -> bool {
        self.queue
            .front()
            .is_some_and(|(_, needs)| *needs <= sync.completed)
    }

    /// Make every held request wait for re-query `needs` (a new connection).
    fn require_sync(&mut self, needs: u64) {
        for (_, queued) in &mut self.queue {
            *queued = needs;
        }
    }

    /// Hold `trigger` until a re-query issued after this call has completed.
    fn push(&mut self, trigger: TriggerEvent, sync: &BundleSync, agent_pubkey: &str) {
        if self.queue.iter().any(|(queued, _)| queued.id == trigger.id) {
            return;
        }
        if self.queue.len() >= START_REQUEST_BACKLOG {
            if let Some((dropped, _)) = self.queue.pop_front() {
                tracing::warn!(
                    agent = %agent_pubkey,
                    event_id = %dropped.id,
                    "start request backlog full; dropping the oldest request for a newer one"
                );
            }
        }
        self.queue.push_back((trigger, sync.issued + 1));
    }

    /// Hand the oldest request to the wake loop through `permit`. A wake loop
    /// that has stopped is not this tap's exit to report — its own task exit
    /// already is — so the backlog is discarded and the tap carries on.
    fn hand_over(
        &mut self,
        permit: Result<mpsc::Permit<'_, TriggerEvent>, mpsc::error::SendError<()>>,
        agent_pubkey: &str,
    ) {
        match permit {
            Ok(permit) => {
                if let Some((trigger, _)) = self.queue.pop_front() {
                    tracing::info!(
                        agent = %agent_pubkey,
                        event_id = %trigger.id,
                        "bundle tap handed an owner start request to the wake loop"
                    );
                    permit.send(trigger);
                }
            }
            Err(_) => {
                tracing::warn!(
                    agent = %agent_pubkey,
                    dropped = self.queue.len(),
                    "the wake loop has stopped; bundle tap cannot hand over start requests"
                );
                self.queue.clear();
            }
        }
    }
}

/// Verify one start-request envelope. Every refusal is logged and dropped —
/// see [`run_bundle_tap`] on junk.
fn verify_start_request(
    keys: &Keys,
    owner_pubkey: &str,
    envelope_author: &nostr::PublicKey,
    event: &nostr::Event,
    agent_pubkey: &str,
) -> Option<TriggerEvent> {
    match admit_start_request(keys, owner_pubkey, envelope_author, event, now_secs()) {
        Ok(trigger) => trigger,
        Err(error) => {
            tracing::warn!(
                agent = %agent_pubkey,
                event_id = %event.id,
                %error,
                "bundle tap refused a start request"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::start_request::START_REQUEST_SUBSCRIPTION_ID;
    use nostr::{EventBuilder, Kind};

    /// A start-request envelope signed by the pair's envelope key, carrying a
    /// request signed by `signer`, dated `requested_at`.
    fn start_envelope(
        owner: &Keys,
        agent: &Keys,
        signer: &Keys,
        requested_at: u64,
    ) -> nostr::Event {
        use crate::start_request::{SignedStartRequest, StartRequestBody};
        let body = StartRequestBody {
            agent_pubkey: agent.public_key().to_hex(),
            requested_at,
        };
        let signed =
            SignedStartRequest::sign(&body, &signer.secret_key().keypair(nostr::SECP256K1))
                .unwrap();
        let envelope =
            start_request_envelope_keys(owner.secret_key(), &agent.public_key()).unwrap();
        let content = nostr::nips::nip44::encrypt(
            envelope.secret_key(),
            &agent.public_key(),
            serde_json::to_string(&signed).unwrap(),
            nostr::nips::nip44::Version::V2,
        )
        .unwrap();
        EventBuilder::new(Kind::Custom(KIND_WAKER_BUNDLE_ENVELOPE as u16), content)
            .tags([nostr::Tag::public_key(agent.public_key())])
            .sign_with_keys(&envelope)
            .unwrap()
    }

    fn envelope_author(owner: &Keys, agent: &Keys) -> nostr::PublicKey {
        start_request_envelope_keys(agent.secret_key(), &owner.public_key())
            .unwrap()
            .public_key()
    }

    #[test]
    fn only_a_start_request_the_owner_signed_is_admitted() {
        let (owner, agent) = (Keys::generate(), Keys::generate());
        let owner_hex = owner.public_key().to_hex();
        let author = envelope_author(&owner, &agent);

        let forged = start_envelope(&owner, &agent, &Keys::generate(), now_secs());
        assert!(verify_start_request(&agent, &owner_hex, &author, &forged, "agent").is_none());

        let genuine = start_envelope(&owner, &agent, &owner, now_secs());
        let trigger = verify_start_request(&agent, &owner_hex, &author, &genuine, "agent")
            .expect("the owner's request is admitted");
        assert_eq!(trigger.id, genuine.id.to_hex());
        assert_eq!(trigger.author, owner_hex);
    }

    fn trigger(id: &str) -> TriggerEvent {
        TriggerEvent {
            id: id.to_string(),
            author: String::new(),
            kind: 0,
            p_tags: Vec::new(),
            created_at: 0,
        }
    }

    #[test]
    fn a_fresh_request_survives_a_backlog_full_of_replays() {
        // Round 1 of #170: a reconnect replays requests the cursor already
        // holds, and the owner's new press arrives behind them. The backlog
        // drops the oldest, never the newest, and ignores a second copy.
        let mut pending = PendingStartRequests::default();
        let sync = BundleSync::default();
        for i in 0..START_REQUEST_BACKLOG {
            pending.push(trigger(&format!("replay-{i}")), &sync, "agent");
        }
        pending.push(trigger("replay-5"), &sync, "agent");
        pending.push(trigger("fresh"), &sync, "agent");

        assert_eq!(pending.queue.len(), START_REQUEST_BACKLOG);
        assert_eq!(pending.queue.front().unwrap().0.id, "replay-1");
        assert_eq!(pending.queue.back().unwrap().0.id, "fresh");
    }

    #[test]
    fn a_request_waits_for_a_re_query_issued_after_it_was_read() {
        let mut pending = PendingStartRequests::default();
        let mut sync = BundleSync::default();
        pending.push(trigger("first"), &sync, "agent");
        assert!(!pending.ready(&sync));
        assert!(pending.needs_sync(&sync));

        // Re-query 1 goes out; a request read while it is in flight needs 2.
        sync.issued = 1;
        sync.in_flight = true;
        pending.push(trigger("second"), &sync, "agent");
        assert!(!pending.needs_sync(&sync), "one re-query at a time");

        sync.completed = 1;
        sync.in_flight = false;
        assert!(pending.ready(&sync), "the first request's re-query is done");
        assert!(pending.needs_sync(&sync), "the second still needs its own");

        // A reconnect starts the count again: nothing held is ready.
        pending.require_sync(1);
        let sync = BundleSync::default();
        assert!(!pending.ready(&sync));
        assert!(pending.needs_sync(&sync));
    }

    fn sealed_bundle(owner: &Keys, agent: &Keys, version: u64, revoked: bool) -> String {
        use crate::bundle::{LaunchBundleBody, ProviderEnvelope};
        let body = LaunchBundleBody {
            agent_pubkey: agent.public_key().to_hex(),
            agent_json: serde_json::json!({"launch": {"policy_env": {}}}),
            provider: ProviderEnvelope {
                provider_id: "sprites".to_string(),
                provider_config: serde_json::json!({}),
                provider_binary_sha256_by_target: crate::bundle::test_digests(&"b".repeat(64)),
            },
            bundle_version: version,
            issued_at: 0,
            expires_at: u64::MAX,
            owner_only_access: true,
            revoked,
        };
        let keypair =
            nostr::secp256k1::Keypair::from_secret_key(nostr::SECP256K1, owner.secret_key());
        let signed = SignedLaunchBundle::sign(&body, &keypair).unwrap();
        nostr::nips::nip44::encrypt(
            owner.secret_key(),
            &agent.public_key(),
            serde_json::to_string(&signed).unwrap(),
            nostr::nips::nip44::Version::V2,
        )
        .unwrap()
    }

    /// A relay double that completes NIP-42, waits for the tap's two REQs,
    /// then delivers `frames` in order and holds the socket open. It answers
    /// every bundle re-query with `sync_events`, then EOSE.
    async fn scripted_relay(frames: Vec<Value>, sync_events: Vec<nostr::Event>) -> String {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let challenge = json!(["AUTH", "challenge"]).to_string();
            ws.send(WsMessage::Text(challenge.into())).await.unwrap();
            let mut reqs = 0;
            while reqs < 2 {
                let Some(Ok(WsMessage::Text(text))) = ws.next().await else {
                    return;
                };
                let frame: Vec<Value> = serde_json::from_str(&text).unwrap();
                match frame[0].as_str() {
                    Some("AUTH") => {
                        let ok = json!(["OK", frame[1]["id"], true, ""]).to_string();
                        ws.send(WsMessage::Text(ok.into())).await.unwrap();
                    }
                    Some("REQ") => reqs += 1,
                    _ => {}
                }
            }
            for frame in frames {
                ws.send(WsMessage::Text(frame.to_string().into()))
                    .await
                    .unwrap();
            }
            while let Some(Ok(message)) = ws.next().await {
                let WsMessage::Text(text) = message else {
                    continue;
                };
                let frame: Vec<Value> = serde_json::from_str(&text).unwrap();
                if frame[0] != "REQ" || frame[1] != BUNDLE_SYNC_SUBSCRIPTION_ID {
                    continue;
                }
                for event in &sync_events {
                    let frame = json!(["EVENT", BUNDLE_SYNC_SUBSCRIPTION_ID, event]);
                    ws.send(WsMessage::Text(frame.to_string().into()))
                        .await
                        .unwrap();
                }
                let eose = json!(["EOSE", BUNDLE_SYNC_SUBSCRIPTION_ID]).to_string();
                ws.send(WsMessage::Text(eose.into())).await.unwrap();
            }
        });
        format!("ws://{addr}")
    }

    /// Owner, agent, and a floor store that has admitted bundle 1, which is
    /// also the tap's cached bundle.
    fn admitted_bundle_fixture() -> (Keys, Keys, tempfile::TempDir, FloorStore, Arc<BundleState>) {
        let (owner, agent) = (Keys::generate(), Keys::generate());
        let owner_hex = owner.public_key().to_hex();
        let dir = tempfile::tempdir().unwrap();
        let mut floor_store =
            FloorStore::enroll(dir.path().join("floor.json"), &owner_hex).unwrap();
        let admitted = decrypt_verify_and_admit(
            &agent,
            &owner_hex,
            &sealed_bundle(&owner, &agent, 1, false),
            &mut floor_store,
        );
        let Ok(BundleOutcome::Delivered(body)) = admitted else {
            panic!("the first bundle is admitted: {admitted:?}");
        };
        let state = Arc::new(BundleState::new());
        state.set(body);
        (owner, agent, dir, floor_store, state)
    }

    fn spawn_tap(
        relay: String,
        agent: Keys,
        owner_hex: String,
        mut floor_store: FloorStore,
        state: &Arc<BundleState>,
        tx: mpsc::Sender<TriggerEvent>,
        cancel: &CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let (state, cancel) = (Arc::clone(state), cancel.clone());
        tokio::spawn(async move {
            run_bundle_tap(
                &relay,
                &agent,
                None,
                &owner_hex,
                &mut floor_store,
                &state,
                &tx,
                &cancel,
            )
            .await;
        })
    }

    /// Round 2 of #170, P1. The wake loop is not taking requests, the owner
    /// presses Start, then revokes the bundle. The tap must still read the
    /// revocation, so that by the time the loop takes the request there is no
    /// bundle left for the attempt to deploy.
    #[tokio::test]
    async fn a_revocation_behind_a_stalled_request_handoff_still_clears_the_bundle() {
        let (owner, agent, _dir, floor_store, state) = admitted_bundle_fixture();
        let owner_hex = owner.public_key().to_hex();
        let agent_hex = agent.public_key().to_hex();

        let press = start_envelope(&owner, &agent, &owner, now_secs());
        let revocation = bundle_event(&owner, &sealed_bundle(&owner, &agent, 2, true), &agent_hex);
        let relay = scripted_relay(
            vec![
                json!(["EVENT", START_REQUEST_SUBSCRIPTION_ID, press]),
                json!(["EVENT", BUNDLE_TAP_SUBSCRIPTION_ID, revocation]),
            ],
            Vec::new(),
        )
        .await;

        // One slot, already taken: the wake loop is not draining.
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(trigger("backlog")).await.unwrap();
        let cancel = CancellationToken::new();
        let tap = spawn_tap(relay, agent, owner_hex, floor_store, &state, tx, &cancel);

        tokio::time::timeout(Duration::from_secs(5), async {
            while state.current().is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a stalled request handoff must not stop the tap reading the revocation");

        // The loop comes back: the press is still delivered, and the bundle
        // an attempt would snapshot for it is already gone.
        assert_eq!(rx.recv().await.unwrap().id, "backlog");
        let taken = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(taken.id, press.id.to_hex());
        assert!(state.current().is_none());

        cancel.cancel();
        tap.await.unwrap();
    }

    /// Round 3 of #170. A revocation the relay sent before a request is
    /// applied before that request can reach the wake loop, even when the
    /// loop is waiting on the channel and would snapshot at once.
    #[tokio::test]
    async fn a_revocation_sent_before_a_request_is_applied_before_it_is_handed_over() {
        let (owner, agent, _dir, floor_store, state) = admitted_bundle_fixture();
        let owner_hex = owner.public_key().to_hex();
        let agent_hex = agent.public_key().to_hex();

        let press = start_envelope(&owner, &agent, &owner, now_secs());
        let revocation = bundle_event(&owner, &sealed_bundle(&owner, &agent, 2, true), &agent_hex);
        let relay = scripted_relay(
            vec![
                json!(["EVENT", BUNDLE_TAP_SUBSCRIPTION_ID, revocation]),
                json!(["EVENT", START_REQUEST_SUBSCRIPTION_ID, press]),
            ],
            Vec::new(),
        )
        .await;

        let (tx, mut rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let tap = spawn_tap(relay, agent, owner_hex, floor_store, &state, tx, &cancel);

        let taken = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(taken.id, press.id.to_hex());
        assert!(
            state.current().is_none(),
            "the attempt for this press would snapshot the revoked bundle"
        );

        cancel.cancel();
        tap.await.unwrap();
    }

    /// Round 4 of #170, P1. The relay runs each REQ in its own task, so on a
    /// reconnect the start-request history can arrive before the bundle
    /// history, even though the owner revoked before pressing Start. Here the
    /// bundle subscription never delivers the revocation at all: only the
    /// re-query the request waits for carries it.
    #[tokio::test]
    async fn a_request_read_ahead_of_an_earlier_revocation_waits_for_it() {
        let (owner, agent, _dir, floor_store, state) = admitted_bundle_fixture();
        let owner_hex = owner.public_key().to_hex();
        let agent_hex = agent.public_key().to_hex();

        let press = start_envelope(&owner, &agent, &owner, now_secs());
        let revocation = bundle_event(&owner, &sealed_bundle(&owner, &agent, 2, true), &agent_hex);
        let relay = scripted_relay(
            vec![json!(["EVENT", START_REQUEST_SUBSCRIPTION_ID, press])],
            vec![revocation],
        )
        .await;

        let (tx, mut rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let tap = spawn_tap(relay, agent, owner_hex, floor_store, &state, tx, &cancel);

        let taken = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(taken.id, press.id.to_hex());
        assert!(
            state.current().is_none(),
            "the attempt for this press would snapshot the revoked bundle"
        );

        cancel.cancel();
        tap.await.unwrap();
    }

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
                provider_binary_sha256_by_target: crate::bundle::test_digests(&"b".repeat(64)),
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
                provider_binary_sha256_by_target: std::collections::BTreeMap::new(),
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

    /// The reconnect regression: a disable at v2 followed by a re-enable at
    /// v3 leaves both envelopes on the relay (the wire kind isn't
    /// replaceable), and on reconnect the relay returns history in
    /// `created_at DESC` order — v3 (the current, valid bundle) before v2
    /// (the stale revocation that predates it). Processing v3 first and then
    /// v2 must not clear the cache v3 just populated.
    #[test]
    fn a_stale_revocation_in_reverse_history_order_does_not_clear_a_newer_admitted_bundle() {
        use crate::bundle::{LaunchBundleBody, ProviderEnvelope};

        let owner = Keys::generate();
        let owner_pubkey = normalize_pubkey(&owner.public_key().to_hex());
        let agent = Keys::generate();
        let dir = tempfile::tempdir().unwrap();
        let mut floor_store =
            FloorStore::enroll(dir.path().join("floor.json"), &owner_pubkey).unwrap();
        let owner_keypair =
            nostr::secp256k1::Keypair::from_secret_key(nostr::SECP256K1, owner.secret_key());

        let encrypt = |body: &LaunchBundleBody| -> String {
            let signed = SignedLaunchBundle::sign(body, &owner_keypair).unwrap();
            let plaintext = serde_json::to_string(&signed).unwrap();
            nostr::nips::nip44::encrypt(
                owner.secret_key(),
                &agent.public_key(),
                &plaintext,
                nostr::nips::nip44::Version::V2,
            )
            .unwrap()
        };

        let v2_revocation = LaunchBundleBody {
            agent_pubkey: agent.public_key().to_hex(),
            agent_json: serde_json::Value::Null,
            provider: ProviderEnvelope {
                provider_id: String::new(),
                provider_config: serde_json::json!({}),
                provider_binary_sha256_by_target: std::collections::BTreeMap::new(),
            },
            bundle_version: 2,
            issued_at: 0,
            expires_at: u64::MAX,
            owner_only_access: true,
            revoked: true,
        };
        let v3_reissue = LaunchBundleBody {
            agent_json: serde_json::json!({"launch": {"policy_env": {}}}),
            provider: ProviderEnvelope {
                provider_id: "sprites".to_string(),
                provider_config: serde_json::json!({}),
                provider_binary_sha256_by_target: crate::bundle::test_digests(&"b".repeat(64)),
            },
            bundle_version: 3,
            revoked: false,
            ..v2_revocation.clone()
        };

        let v2_ciphertext = encrypt(&v2_revocation);
        let v3_ciphertext = encrypt(&v3_reissue);

        // Reconnect order: newest first.
        let admitted =
            decrypt_verify_and_admit(&agent, &owner_pubkey, &v3_ciphertext, &mut floor_store)
                .expect("v3 admits");
        assert_eq!(
            admitted,
            BundleOutcome::Delivered(
                SignedLaunchBundle::sign(&v3_reissue, &owner_keypair)
                    .unwrap()
                    .verify(&owner_pubkey, u64::MAX)
                    .expect("the same body the tap just admitted")
            )
        );
        assert_eq!(floor_store.snapshot().highest_accepted_version, 3);

        let outcome =
            decrypt_verify_and_admit(&agent, &owner_pubkey, &v2_ciphertext, &mut floor_store)
                .expect("a stale revocation is not an error");
        assert_eq!(
            outcome,
            BundleOutcome::StaleRevocation,
            "a revocation superseded by an already-admitted bundle must not report Revoked"
        );
        assert_eq!(
            floor_store.snapshot().highest_accepted_version,
            3,
            "the newer real bundle must remain the admitted version"
        );
        assert_eq!(
            floor_store.snapshot().revocation_floor,
            2,
            "the floor still moves, as defense for any future delivery below it"
        );
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
                provider_binary_sha256_by_target: crate::bundle::test_digests(&"b".repeat(64)),
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
