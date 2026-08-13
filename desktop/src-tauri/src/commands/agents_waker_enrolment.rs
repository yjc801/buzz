//! Publishing enrolment to `buzz-waker`: which agents an owner has enrolled,
//! and the credentials to watch them with.
//!
//! "Owner" is this codebase's term for the key that signs an agent's events —
//! the **user's** own desktop key. It is not the person running the waker.
//! There are two roles: a **service provider** who stands the daemon up and
//! configures it (identity, relay, provider token), and **users** who toggle
//! Remote wake and configure nothing. Nothing published here carries a
//! provider token, because a user has none to carry.
//!
//! Sibling of `agents::waker` (`agents_waker.rs`), which issues the *launch
//! bundle*. The bundle answers "how do I deploy this agent"; enrolment answers
//! "which agents am I watching, and as whom" — the half that today's daemon
//! can only learn from `WAKER_AGENTS_CONFIG_JSON`, hand-assembled and set as a
//! Fly secret. See `docs/waker-agent-enrolment.md`.
//!
//! Both payloads travel the same `KIND_WAKER_BUNDLE_ENVELOPE` (kind 1059) the
//! launch bundle uses and are retained through the same `pending_sync` row the
//! shared flush loop already drains, so publishing, retries, and WebSocket
//! routing come for free. What distinguishes the three streams on the relay is
//! the tag pair:
//!
//! | payload | `p` (recipient) | `d` (coordinate) |
//! |---|---|---|
//! | launch bundle | the **agent** | agent pubkey |
//! | credential | the **waker** | agent pubkey |
//! | roster | the **waker** | [`ROSTER_D_TAG`] |
//!
//! # Why the retention `d_tag` is namespaced
//!
//! The retention store's primary key is `(kind, pubkey, d_tag)`, with no `p`
//! column — so a credential stored under the bare agent pubkey would land on
//! the *same* row as that agent's launch bundle and silently displace it,
//! leaving whichever was written second as the only one ever published. The
//! two are only distinguishable on the relay, not in this table. Rows are
//! therefore keyed by [`credential_retention_d_tag`], while the published
//! event keeps the real `d` tag the daemon filters on: `raw_event` is what the
//! flush loop publishes verbatim, so the local key is free to differ.

use std::collections::BTreeMap;

use nostr::Keys;
use tauri::AppHandle;

use crate::managed_agents::waker_enrolment::ROSTER_D_TAG;
use crate::{
    app_state::AppState,
    managed_agents::{BackendKind, ManagedAgentRecord},
};

/// The retention-store key for one agent's credential row.
///
/// Prefixed so it can never equal a bare 64-hex agent pubkey, which is what
/// the launch bundle's own row is keyed by — see the module doc.
#[must_use]
fn credential_retention_d_tag(agent_pubkey: &str) -> String {
    format!("credential:{agent_pubkey}")
}

/// Issue this agent's credential and republish the owner's roster.
///
/// Called from the same places that reissue a launch bundle — enrolment and
/// every subsequent config change — and a no-op for a `Local` backend, which a
/// remote daemon has nothing to invoke.
///
/// A no-op too when no waker identity is configured, which is every install
/// that does not use a remote waker. There is nothing to encrypt *to* in that
/// case, and an owner who never asked for one should not see an error about it.
///
/// # Errors
/// Propagates a failure to resolve the retention scope, reserve a version,
/// sign, or retain. Callers in the best-effort retain path log and swallow;
/// the revocation path does not — see [`revoke_waker_enrolment_pending`].
pub(crate) fn retain_waker_enrolment_pending(
    app: &AppHandle,
    state: &AppState,
    record: &ManagedAgentRecord,
) -> Result<(), String> {
    use crate::managed_agents::waker_enrolment::{
        enrolment_ledger, parse_auth_tag_elements, waker_identity_pubkey,
    };

    let Some(waker_pubkey) = waker_identity_pubkey() else {
        return Ok(());
    };
    if !matches!(record.backend, BackendKind::Provider { .. }) {
        return Ok(());
    }

    let scope = crate::managed_agents::retention::active_retention_scope(app, state)?;
    let ledger = enrolment_ledger(app)?;
    let issued_at = now_unix()?;

    // The credential first, the roster second, and never the reverse: the
    // roster names the credential version each agent is expected to have, so a
    // roster written first would advertise a version no credential carries. In
    // that order a partial failure leaves a credential no roster mentions —
    // undiscovered and inert — rather than a roster pointing at nothing.
    sign_and_retain_credential_at(
        &scope.db_path,
        &scope.owner_keys,
        &ledger,
        &waker_pubkey,
        &record.pubkey,
        record.private_key_nsec.clone(),
        parse_auth_tag_elements(record.auth_tag.as_deref())?,
        issued_at,
        false,
    )?;

    retain_roster_at(
        app,
        &scope.db_path,
        &scope.owner_keys,
        &ledger,
        &waker_pubkey,
        None,
        issued_at,
    )
}

/// Withdraw an agent from enrolment: a revoked credential, and a roster that
/// no longer lists it.
///
/// Both halves matter and they are not redundant. Roster omission is what
/// stops the daemon watching the agent at all — absence *is* removal. The
/// revoked credential is what reaches a daemon already holding the old one: it
/// raises that agent's durable credential floor, so the superseded credential
/// is refused from then on even though it is still readable on the relay.
///
/// Like [`super::agents::waker::revoke_waker_bundle_pending`](super::agents),
/// the credential half below is **not** best-effort: revocation is the
/// security effect of the calling command, so a caller must propagate `Err`
/// and refuse to persist the disable rather than report success with the agent
/// still enrolled. Call it *before* mutating and saving the record — a
/// failure there needs no rollback, and `exclude` below is what keeps the
/// roster correct while the store still says the agent is enabled.
///
/// The roster half that follows is best-effort once the credential half has
/// landed. The credential retain is what queues the revocation for publish —
/// by the time it succeeds the security effect is already committed and
/// cannot be un-queued, so a caller that then refused to persist the disable
/// because the roster write failed would report "waker remains enabled" while
/// the in-flight revocation tears the agent down regardless: local state and
/// the external effect would claim opposites. A roster that still names this
/// agent self-corrects the next time any other agent's enrolment is retained,
/// since [`enrolled_credential_versions`] filters on `waker_enabled`, which
/// the caller persists as `false` once this returns `Ok`.
///
/// # Errors
/// Propagates a failure to resolve the scope, reserve a version, or sign or
/// retain the credential revocation. A roster retract failure after that is
/// logged and swallowed, not propagated.
pub(crate) fn revoke_waker_enrolment_pending(
    app: &AppHandle,
    state: &AppState,
    agent_pubkey: &str,
) -> Result<(), String> {
    use crate::managed_agents::waker_enrolment::{enrolment_ledger, waker_identity_pubkey};

    let Some(waker_pubkey) = waker_identity_pubkey() else {
        return Ok(());
    };

    let scope = crate::managed_agents::retention::active_retention_scope(app, state)?;
    let ledger = enrolment_ledger(app)?;
    let issued_at = now_unix()?;

    sign_and_retain_credential_at(
        &scope.db_path,
        &scope.owner_keys,
        &ledger,
        &waker_pubkey,
        agent_pubkey,
        // Unread by a revoked delivery — see `CredentialBody::revoked`. Empty
        // rather than the real key: a revocation that still carried a usable
        // nsec would put the agent's private key on the relay for no reason.
        String::new(),
        None,
        issued_at,
        true,
    )?;

    // The record still says `waker_enabled` at this point by design, so the
    // agent is excluded explicitly rather than by reloading the store.
    if let Err(e) = retain_roster_at(
        app,
        &scope.db_path,
        &scope.owner_keys,
        &ledger,
        &waker_pubkey,
        Some(agent_pubkey),
        issued_at,
    ) {
        eprintln!("buzz-desktop: waker-enrolment-roster-retract: {e}");
    }

    Ok(())
}

/// Every agent this owner currently has enrolled, with the credential version
/// each one holds.
///
/// Only agents with an issued credential (version ≥ 1) are listed. A roster
/// entry is a promise that a credential exists at that version, so listing an
/// agent at version 0 would send the daemon to open a per-agent credential tap
/// against a coordinate nothing has been published to.
///
/// `exclude` drops one agent regardless of what the store says, for the
/// revocation path that runs before the record is saved.
fn enrolled_credential_versions(
    app: &AppHandle,
    ledger: &crate::managed_agents::waker_bundle::IssuanceLedger,
    exclude: Option<&str>,
) -> Result<BTreeMap<String, u64>, String> {
    let candidates = crate::managed_agents::storage::load_managed_agents(app)?
        .into_iter()
        .filter(|record| {
            record.waker_enabled && matches!(record.backend, BackendKind::Provider { .. })
        })
        .map(|record| record.pubkey);
    credential_versions_for(candidates, ledger, exclude)
}

/// The pure half of [`enrolled_credential_versions`]: drop `exclude`, look up
/// each remaining agent's issued credential version, and keep only those that
/// have one. Split from the store read so the two rules that matter — an
/// excluded agent never appears, and an agent with no credential never appears
/// — are testable without a Tauri `AppHandle`.
fn credential_versions_for(
    candidates: impl IntoIterator<Item = String>,
    ledger: &crate::managed_agents::waker_bundle::IssuanceLedger,
    exclude: Option<&str>,
) -> Result<BTreeMap<String, u64>, String> {
    let mut versions = BTreeMap::new();
    for agent_pubkey in candidates {
        if exclude.is_some_and(|excluded| excluded == agent_pubkey) {
            continue;
        }
        let version = ledger.committed(&agent_pubkey)?;
        if version > 0 {
            versions.insert(agent_pubkey, version);
        }
    }
    Ok(versions)
}

/// Unix seconds, or a clear error if the clock is before the epoch.
fn now_unix() -> Result<u64, String> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("system clock before unix epoch: {e}"))?
        .as_secs())
}

/// Build, sign, and retain this owner's full roster at its fixed coordinate.
fn retain_roster_at(
    app: &AppHandle,
    db_path: &std::path::Path,
    owner_keys: &Keys,
    ledger: &crate::managed_agents::waker_bundle::IssuanceLedger,
    waker_pubkey: &str,
    exclude: Option<&str>,
    issued_at: u64,
) -> Result<(), String> {
    use crate::managed_agents::waker_enrolment::sign_enrolment_roster;

    let versions = enrolled_credential_versions(app, ledger, exclude)?;
    let roster_version = ledger.reserve(ROSTER_D_TAG)?;
    let signed = sign_enrolment_roster(&versions, roster_version, issued_at, owner_keys)?;
    let plaintext =
        serde_json::to_string(&signed).map_err(|e| format!("failed to serialize roster: {e}"))?;

    retain_enrolment_envelope(
        db_path,
        owner_keys,
        waker_pubkey,
        ROSTER_D_TAG,
        ROSTER_D_TAG,
        &plaintext,
    )
}

/// The pure half of [`retain_waker_enrolment_pending`]: reserve a version,
/// sign, encrypt to the waker, and retain. Split out so the round-trip against
/// the daemon's real verifier is testable against a tempdir without a Tauri
/// `AppHandle`, mirroring `waker::sign_and_retain_waker_bundle_at`.
#[allow(clippy::too_many_arguments)]
pub(super) fn sign_and_retain_credential_at(
    db_path: &std::path::Path,
    owner_keys: &Keys,
    ledger: &crate::managed_agents::waker_bundle::IssuanceLedger,
    waker_pubkey: &str,
    agent_pubkey: &str,
    nsec: String,
    auth_tag: Option<Vec<String>>,
    issued_at: u64,
    revoked: bool,
) -> Result<(), String> {
    use crate::managed_agents::waker_enrolment::{sign_enrolment_credential, CredentialInputs};

    let credential_version = ledger.reserve(agent_pubkey)?;
    let committed_version = credential_version.number();
    let signed = sign_enrolment_credential(
        CredentialInputs {
            agent_pubkey: agent_pubkey.to_string(),
            nsec,
            auth_tag,
            credential_version,
            issued_at,
            revoked,
        },
        owner_keys,
    )?;
    let plaintext = serde_json::to_string(&signed)
        .map_err(|e| format!("failed to serialize credential: {e}"))?;

    retain_enrolment_envelope(
        db_path,
        owner_keys,
        waker_pubkey,
        agent_pubkey,
        &credential_retention_d_tag(agent_pubkey),
        &plaintext,
    )?;

    // Only past this point has a credential actually been retained at
    // `committed_version` — see `IssuanceLedger::committed`. A roster built
    // from `ledger.current` instead would advertise this version even when an
    // earlier step above failed and left nothing retained.
    ledger.record_committed(agent_pubkey, committed_version)
}

/// Encrypt `plaintext` to the waker, wrap it in the shared envelope kind, and
/// retain it for the flush loop.
///
/// `event_d_tag` is what the daemon's `#d` filter matches; `retention_d_tag`
/// is this store's own key for the row. They differ for credentials — see the
/// module doc on why that collision has to be broken locally.
fn retain_enrolment_envelope(
    db_path: &std::path::Path,
    owner_keys: &Keys,
    waker_pubkey: &str,
    event_d_tag: &str,
    retention_d_tag: &str,
    plaintext: &str,
) -> Result<(), String> {
    use crate::managed_agents::{
        persona_events::monotonic_created_at,
        retention::{get_retained_event, open_retention_db, retain_event, RetainedEvent},
    };
    use buzz_core_pkg::kind::KIND_WAKER_BUNDLE_ENVELOPE;
    use nostr::{nips::nip44, JsonUtil, PublicKey, Tag};

    let waker_pk = PublicKey::from_hex(waker_pubkey)
        .map_err(|e| format!("invalid waker identity pubkey: {e}"))?;
    let ciphertext = nip44::encrypt(
        owner_keys.secret_key(),
        &waker_pk,
        plaintext,
        nip44::Version::V2,
    )
    .map_err(|e| format!("failed to encrypt the enrolment payload: {e}"))?;

    let conn = open_retention_db(db_path)?;
    let owner_pubkey = owner_keys.public_key().to_hex();
    // Same monotonic bump as the launch bundle's row, and for the same reason:
    // `retain_event` only replaces when `created_at` does not go backwards, and
    // wall-clock seconds alone let a same-second reissue tie — which would drop
    // the newer payload on the floor and never queue it for publish. Relay-side
    // ordering is irrelevant here; the envelope is not
    // parameterized-replaceable, and the daemon's floors are what order these.
    let existing = get_retained_event(
        &conn,
        KIND_WAKER_BUNDLE_ENVELOPE,
        &owner_pubkey,
        retention_d_tag,
    )?;

    let event = nostr::EventBuilder::new(
        nostr::Kind::Custom(KIND_WAKER_BUNDLE_ENVELOPE as u16),
        ciphertext,
    )
    .tags([
        Tag::parse(["d", event_d_tag]).map_err(|e| e.to_string())?,
        // The recipient is the waker, not the agent: this is the one tag that
        // separates an enrolment payload from a launch bundle on the relay.
        Tag::parse(["p", waker_pubkey]).map_err(|e| e.to_string())?,
    ])
    .custom_created_at(monotonic_created_at(
        existing.as_ref().map(|row| row.created_at),
    ))
    .sign_with_keys(owner_keys)
    .map_err(|e| format!("failed to sign the enrolment event: {e}"))?;

    retain_event(
        &conn,
        &RetainedEvent {
            kind: KIND_WAKER_BUNDLE_ENVELOPE,
            pubkey: owner_pubkey,
            d_tag: retention_d_tag.to_string(),
            content: event.content.to_string(),
            created_at: event.created_at.as_secs() as i64,
            raw_event: event.as_json(),
            pending_sync: true,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_agents::retention::{get_retained_event, open_retention_db};
    use crate::managed_agents::waker_bundle::IssuanceLedger;
    use buzz_core_pkg::kind::KIND_WAKER_BUNDLE_ENVELOPE;

    fn ledger(dir: &std::path::Path) -> IssuanceLedger {
        IssuanceLedger::open(dir.join("waker-enrolment-versions.json"))
    }

    /// Decrypt a retained row as the waker identity would, and hand back the
    /// still-signed payload for the daemon's own verifier to check.
    fn decrypt_as_waker(
        db_path: &std::path::Path,
        owner: &Keys,
        waker: &Keys,
        retention_d_tag: &str,
    ) -> String {
        let conn = open_retention_db(db_path).expect("open db");
        let retained = get_retained_event(
            &conn,
            KIND_WAKER_BUNDLE_ENVELOPE,
            &owner.public_key().to_hex(),
            retention_d_tag,
        )
        .expect("query")
        .expect("a row was retained at this coordinate");
        assert!(retained.pending_sync, "must be queued for the flush loop");
        nostr::nips::nip44::decrypt(waker.secret_key(), &owner.public_key(), &retained.content)
            .expect("the waker must be able to decrypt what the owner encrypted to it")
    }

    /// The headline case: what the desktop retains round-trips through the
    /// exact decrypt-and-verify path the daemon's credential tap runs.
    #[test]
    fn a_retained_credential_decrypts_and_verifies_as_the_waker_would() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("retention.sqlite");
        let owner = Keys::generate();
        let waker = Keys::generate();
        let agent = Keys::generate();
        let agent_pubkey = agent.public_key().to_hex();

        sign_and_retain_credential_at(
            &db_path,
            &owner,
            &ledger(dir.path()),
            &waker.public_key().to_hex(),
            &agent_pubkey,
            // A real key that derives `agent_pubkey`: the daemon refuses a
            // live credential whose nsec does not parse (`MalformedNsec`) or
            // derives a different pubkey than the body claims
            // (`CredentialKeyMismatch`), so a placeholder here would test
            // nothing the daemon would ever accept.
            nostr::ToBech32::to_bech32(agent.secret_key()).expect("bech32"),
            Some(vec![
                "auth".into(),
                "b".repeat(64),
                "sig".into(),
                String::new(),
            ]),
            1_000,
            false,
        )
        .expect("retain");

        let plaintext = decrypt_as_waker(
            &db_path,
            &owner,
            &waker,
            &credential_retention_d_tag(&agent_pubkey),
        );
        let signed: buzz_waker_pkg::SignedCredential =
            serde_json::from_str(&plaintext).expect("parse signed credential");
        let body = signed
            .verify(&[owner.public_key().to_hex()])
            .expect("the waker must accept what the desktop retained");

        assert_eq!(body.agent_pubkey, agent_pubkey);
        assert_eq!(body.credential_version, 1);
        assert!(
            body.provider_credential.is_none(),
            "a user has no provider token to send; the service provider's own \
             credential stays in the daemon's environment and never transits the relay"
        );
    }

    /// The collision this module exists to avoid: a credential must not
    /// displace that agent's launch bundle, which is keyed by the bare pubkey
    /// at the same `(kind, pubkey, d_tag)` primary key.
    #[test]
    fn a_credential_does_not_displace_the_agents_launch_bundle_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("retention.sqlite");
        let owner = Keys::generate();
        let waker = Keys::generate();
        let agent = Keys::generate();
        let agent_pubkey = agent.public_key().to_hex();

        crate::commands::agents::waker::sign_and_retain_waker_bundle_at(
            &db_path,
            &owner,
            &IssuanceLedger::open(dir.path().join("waker-bundle-versions.json")),
            &agent_pubkey,
            serde_json::json!({"launch": {"command": "buzz-acp"}}),
            "sprites".to_string(),
            serde_json::json!({"org": "buzz-team"}),
            std::collections::BTreeMap::from([(
                "x86_64-unknown-linux-musl".to_string(),
                "b".repeat(64),
            )]),
            1_000,
            false,
        )
        .expect("retain the launch bundle");

        sign_and_retain_credential_at(
            &db_path,
            &owner,
            &ledger(dir.path()),
            &waker.public_key().to_hex(),
            &agent_pubkey,
            "nsec1fake".to_string(),
            None,
            1_000,
            false,
        )
        .expect("retain the credential");

        let conn = open_retention_db(&db_path).expect("open db");
        let bundle_row = get_retained_event(
            &conn,
            KIND_WAKER_BUNDLE_ENVELOPE,
            &owner.public_key().to_hex(),
            &agent_pubkey,
        )
        .expect("query")
        .expect("the launch bundle row must survive issuing a credential");

        // The bundle is encrypted to the agent; the credential to the waker.
        // Decrypting as the agent proves this row is still the bundle.
        let plaintext = nostr::nips::nip44::decrypt(
            agent.secret_key(),
            &owner.public_key(),
            &bundle_row.content,
        )
        .expect("the surviving row must still be the agent-encrypted bundle");
        let signed: buzz_waker_pkg::SignedLaunchBundle =
            serde_json::from_str(&plaintext).expect("parse");
        assert!(
            signed.verify(&owner.public_key().to_hex(), 2_000).is_ok(),
            "the launch bundle must be intact after a credential is retained"
        );
    }

    /// Reissuing must never repeat a credential version — the same invariant
    /// the launch bundle ledger has, and for the same reason: a repeat lets
    /// the daemon's floor treat a new body as a routine redelivery.
    #[test]
    fn reissuing_a_credential_never_repeats_a_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("retention.sqlite");
        let owner = Keys::generate();
        let waker = Keys::generate();
        let agent = Keys::generate();
        let agent_pubkey = agent.public_key().to_hex();
        let nsec = nostr::ToBech32::to_bech32(agent.secret_key()).expect("bech32");
        let ledger = ledger(dir.path());

        for issued_at in [1_000, 2_000] {
            sign_and_retain_credential_at(
                &db_path,
                &owner,
                &ledger,
                &waker.public_key().to_hex(),
                &agent_pubkey,
                nsec.clone(),
                None,
                issued_at,
                false,
            )
            .expect("retain");
        }

        let plaintext = decrypt_as_waker(
            &db_path,
            &owner,
            &waker,
            &credential_retention_d_tag(&agent_pubkey),
        );
        let signed: buzz_waker_pkg::SignedCredential = serde_json::from_str(&plaintext).unwrap();
        let body = signed.verify(&[owner.public_key().to_hex()]).unwrap();
        assert_eq!(
            body.credential_version, 2,
            "the second issuance must carry version 2, not repeat version 1"
        );
    }

    /// A revocation supersedes the live credential at the same local
    /// coordinate, and verifies as `revoked` with no usable key left in it.
    #[test]
    fn a_revocation_supersedes_the_credential_and_carries_no_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("retention.sqlite");
        let owner = Keys::generate();
        let waker = Keys::generate();
        let agent_pubkey = Keys::generate().public_key().to_hex();
        let ledger = ledger(dir.path());

        sign_and_retain_credential_at(
            &db_path,
            &owner,
            &ledger,
            &waker.public_key().to_hex(),
            &agent_pubkey,
            "nsec1realkey".to_string(),
            None,
            1_000,
            false,
        )
        .expect("issue");

        sign_and_retain_credential_at(
            &db_path,
            &owner,
            &ledger,
            &waker.public_key().to_hex(),
            &agent_pubkey,
            String::new(),
            None,
            2_000,
            true,
        )
        .expect("revoke");

        let plaintext = decrypt_as_waker(
            &db_path,
            &owner,
            &waker,
            &credential_retention_d_tag(&agent_pubkey),
        );
        let signed: buzz_waker_pkg::SignedCredential = serde_json::from_str(&plaintext).unwrap();
        let body = signed.verify(&[owner.public_key().to_hex()]).unwrap();
        assert!(body.revoked, "the surviving row must be the revocation");
        assert_eq!(body.credential_version, 2);
        assert!(
            body.nsec.is_empty(),
            "a revocation must not put the agent's key back on the relay"
        );
    }

    /// Roster membership is what withdraws an agent — absence *is* removal —
    /// so the excluded agent must not survive into the body, even though the
    /// record still says `waker_enabled` when revocation runs.
    #[test]
    fn the_roster_drops_the_revoked_agent_and_any_agent_without_a_credential() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = ledger(dir.path());
        let (kept, revoked, never_issued) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));

        // `kept` and `revoked` have credentials; `never_issued` does not.
        // Reserving alone is not enough — only a committed version (recorded
        // once a credential is actually retained, not merely reserved) counts
        // as issued. See `IssuanceLedger::committed`.
        ledger.reserve(&kept).expect("reserve");
        ledger.record_committed(&kept, 1).expect("commit");
        ledger.reserve(&revoked).expect("reserve");
        ledger.record_committed(&revoked, 1).expect("commit");

        let versions = credential_versions_for(
            [kept.clone(), revoked.clone(), never_issued.clone()],
            &ledger,
            Some(&revoked),
        )
        .expect("build");

        assert_eq!(
            versions.keys().collect::<Vec<_>>(),
            vec![&kept],
            "only the still-enrolled agent with an issued credential belongs"
        );
        assert_eq!(versions[&kept], 1);
    }

    /// A version that was reserved but never committed — the durable-before-
    /// signing gap `IssuanceLedger::reserve` documents, e.g. an invalid waker
    /// key or a retention failure after the reservation lands — must not be
    /// advertised in the roster. A version the roster names is a promise that
    /// a credential exists at it; naming one that was only reserved would send
    /// the daemon to open a tap against a coordinate nothing was ever
    /// published to.
    #[test]
    fn a_reserved_but_never_committed_version_is_absent_from_the_roster() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = ledger(dir.path());
        let agent_pubkey = "a".repeat(64);

        ledger.reserve(&agent_pubkey).expect("reserve");

        let versions =
            credential_versions_for([agent_pubkey.clone()], &ledger, None).expect("build");

        assert!(
            versions.is_empty(),
            "a reserved-only version must not appear in the roster: {versions:?}"
        );
    }

    /// The event the daemon sees must carry the tags its filters pin: `p` at
    /// the waker (not the agent) and `d` at the agent for a credential.
    #[test]
    fn the_published_credential_event_is_tagged_for_the_waker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("retention.sqlite");
        let owner = Keys::generate();
        let waker = Keys::generate();
        let agent_pubkey = Keys::generate().public_key().to_hex();

        sign_and_retain_credential_at(
            &db_path,
            &owner,
            &ledger(dir.path()),
            &waker.public_key().to_hex(),
            &agent_pubkey,
            "nsec1fake".to_string(),
            None,
            1_000,
            false,
        )
        .expect("retain");

        let conn = open_retention_db(&db_path).expect("open db");
        let row = get_retained_event(
            &conn,
            KIND_WAKER_BUNDLE_ENVELOPE,
            &owner.public_key().to_hex(),
            &credential_retention_d_tag(&agent_pubkey),
        )
        .expect("query")
        .expect("retained");

        let event: nostr::Event = nostr::JsonUtil::from_json(&row.raw_event).expect("parse event");
        let tag_values: Vec<Vec<String>> =
            event.tags.iter().map(|tag| tag.clone().to_vec()).collect();
        assert!(
            tag_values.contains(&vec!["p".to_string(), waker.public_key().to_hex()]),
            "the credential must be p-tagged to the waker, not the agent: {tag_values:?}"
        );
        assert!(
            tag_values.contains(&vec!["d".to_string(), agent_pubkey.clone()]),
            "the credential's d tag must be the agent pubkey: {tag_values:?}"
        );
    }
}
