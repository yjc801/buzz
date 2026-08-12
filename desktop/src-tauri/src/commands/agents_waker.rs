//! Issuing and retaining signed launch bundles for `buzz-waker`.
//!
//! The payload is a `KIND_WAKER_LAUNCH_BUNDLE` bundle; the event that carries
//! it to a relay is a `KIND_WAKER_BUNDLE_ENVELOPE` (see that constant for why
//! the two differ). The envelope is *not* parameterized-replaceable, so a
//! reissue or revocation does not displace what came before — it lands beside
//! it. Correctness does not depend on displacement: `buzz-waker`'s `FloorStore`
//! keeps a durable, monotonic revocation floor, so once a revocation is
//! admitted every earlier version is refused permanently, whatever order the
//! daemon happens to read them in.
//!
//! Sibling half of [`super::deploy`]: `build_deploy_payload` there resolves
//! `agent_json`, this module turns that plus the record's provider envelope
//! into something `buzz-waker` can act on. Kept out of `agents.rs` itself,
//! which is already past the desktop file-size ratchet
//! (`desktop/scripts/check-file-sizes.mjs`) and may not grow.

use nostr::Keys;
use tauri::AppHandle;

use crate::{
    app_state::AppState,
    managed_agents::{BackendKind, ManagedAgentRecord},
};

/// Retain a freshly authored managed-agent event in the local store, flagged
/// for relay sync, and — when `record.waker_enabled` — a freshly issued
/// launch bundle alongside it. MUST be called inside the
/// `managed_agents_store_lock`-held body after `save_managed_agents`, NEVER
/// across an `.await`: it acquires `state.keys` and a retention-db
/// connection, both `std::sync` guards, and drops them before returning.
///
/// The 30177 agent-record half is owner-authored, mirroring
/// `commands::personas::retain_persona_pending`: the owner keys sign, the
/// d_tag is the agent's pubkey, so the coordinate is
/// `30177:<owner>:<agent_pubkey>`. The event content is the opt-IN
/// `agent_event_content` projection — the retention upsert's content-equality
/// guard compares this projection, so an operational start/stop that mutates
/// only runtime fields produces an identical row and never re-enqueues a
/// publish. Best-effort: a failure here is logged and swallowed so a
/// retention hiccup never blocks the disk-authoritative write. The
/// [`retain_waker_bundle_pending`] half below applies the same best-effort
/// rule independently.
pub(crate) fn retain_managed_agent_pending(
    app: &AppHandle,
    state: &AppState,
    record: &ManagedAgentRecord,
) {
    use crate::managed_agents::{reconcile::retain_agent_record, retention::open_retention_db};

    let result = (|| -> Result<(), String> {
        let scope = crate::managed_agents::retention::active_retention_scope(app, state)?;
        let conn = open_retention_db(&scope.db_path)?;
        // Shared engine with the boot-time reconcile: projection content diff
        // (no republish for runtime-only churn) + monotonic created_at bump
        // past the retained head (NIP-AP step 3).
        retain_agent_record(&conn, &scope.owner_keys, record).map(|_| ())
    })();
    if let Err(e) = result {
        eprintln!("buzz-desktop: agent-retain: {e}");
    }

    if record.waker_enabled {
        if let Err(e) = retain_waker_bundle_pending(app, state, record) {
            eprintln!("buzz-desktop: waker-bundle-retain: {e}");
        }
    }
}

/// Issue and retain a fresh signed launch bundle for `record`, so
/// `buzz-waker` can deploy it while this desktop is offline.
///
/// Called from every real `retain_managed_agent_pending` site — enrolment
/// (`waker_enabled` flips on) and every subsequent config change — never on a
/// runtime-only churn, matching the same G3 rule (`PLANS/BUZZ_WAKER_DESIGN.md`
/// §11) `retain_managed_agent_pending` itself follows for the 30177 agent
/// record. A no-op for a `Local` backend, which a remote daemon has nothing to
/// invoke.
///
/// Retained (not published directly) through the same `pending_sync` row the
/// generic flush loop (`persona_events::flush_active_pending_events`) already
/// drains every 30s for persona, team, and managed-agent writers — this reuses
/// that retry path rather than adding a second one.
pub(crate) fn retain_waker_bundle_pending(
    app: &AppHandle,
    state: &AppState,
    record: &ManagedAgentRecord,
) -> Result<(), String> {
    use crate::managed_agents::waker_bundle::{issuance_ledger, provider_binary_sha256};

    let BackendKind::Provider {
        id: provider_id,
        config: provider_config,
    } = record.backend.clone()
    else {
        return Ok(());
    };

    let provider_binary_path = record
        .provider_binary_path
        .as_ref()
        .ok_or_else(|| "no provider binary resolved for this agent yet".to_string())?;
    let provider_binary_sha256 =
        provider_binary_sha256(std::path::Path::new(provider_binary_path))?;

    let agent_json = super::deploy::build_deploy_payload(app, state, record, None)?;

    let scope = crate::managed_agents::retention::active_retention_scope(app, state)?;
    let ledger = issuance_ledger(app)?;
    let issued_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("system clock before unix epoch: {e}"))?
        .as_secs();

    sign_and_retain_waker_bundle_at(
        &scope.db_path,
        &scope.owner_keys,
        &ledger,
        &record.pubkey,
        agent_json,
        provider_id,
        provider_config,
        provider_binary_sha256,
        issued_at,
        false,
    )
}

/// Publish an owner-signed revocation for the agent — the disable half of
/// enrolment. It is an ordinary bundle at the next reserved version, carrying
/// `revoked`, published in the same envelope as any reissue.
///
/// It does **not** displace the bundle it revokes. The envelope is not
/// parameterized-replaceable, so the revoked bundle stays readable on the
/// relay; what makes that safe is that the daemon's authority is its own
/// durable state, not the relay's. `bundle_feed::decrypt_verify_and_admit`
/// raises the `FloorStore` revocation floor to this version and clears the
/// cached bundle, and the floor never decreases — so the revoked bundle is
/// refused from then on, including across restarts and regardless of the
/// order the daemon reads the two events in.
///
/// Delivery is live: the daemon's bundle tap holds this filter open ("hold the
/// same filter open live for real-time reissues",
/// `PLANS/BUZZ_WAKER_DESIGN.md` §11), so a revocation reaches an
/// already-connected daemon exactly as a config-change reissue does, with no
/// dependency on it ever reconnecting.
///
/// Called from every place that turns `waker_enabled` off while the agent
/// still has a retained bundle: `set_managed_agent_waker_enabled(false)` and
/// `set_managed_agent_backend` migrating a waker-enabled agent off
/// `Provider`. Unlike every other retention write in this module, this one
/// is **not** best-effort: revocation is the security effect of the calling
/// command, not a side channel, so a caller MUST propagate `Err` and refuse
/// to persist the disable/migration rather than report success with the old
/// bundle still live. Call this *before* mutating and saving the record —
/// that way a failure here needs no rollback, since nothing was written yet.
pub(crate) fn revoke_waker_bundle_pending(
    app: &AppHandle,
    state: &AppState,
    agent_pubkey: &str,
) -> Result<(), String> {
    use crate::managed_agents::waker_bundle::issuance_ledger;

    let scope = crate::managed_agents::retention::active_retention_scope(app, state)?;
    let ledger = issuance_ledger(app)?;
    let issued_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("system clock before unix epoch: {e}"))?
        .as_secs();

    sign_and_retain_waker_bundle_at(
        &scope.db_path,
        &scope.owner_keys,
        &ledger,
        agent_pubkey,
        // Unread by a revoked delivery — see `LaunchBundleBody::revoked`.
        serde_json::Value::Null,
        String::new(),
        serde_json::json!({}),
        String::new(),
        issued_at,
        true,
    )
}

/// The pure half of [`retain_waker_bundle_pending`]: reserve a version, sign,
/// NIP-44-encrypt to the agent, and retain the resulting envelope event —
/// everything after `agent_json` is resolved. Split out so it is unit-testable
/// against a tempdir ledger/retention db without a Tauri `AppHandle`, mirroring
/// `commands::personas::pending::prepare_persona_publication_at`.
#[allow(clippy::too_many_arguments)]
pub(super) fn sign_and_retain_waker_bundle_at(
    db_path: &std::path::Path,
    owner_keys: &Keys,
    ledger: &crate::managed_agents::waker_bundle::IssuanceLedger,
    agent_pubkey: &str,
    agent_json: serde_json::Value,
    provider_id: String,
    provider_config: serde_json::Value,
    provider_binary_sha256: String,
    issued_at: u64,
    revoked: bool,
) -> Result<(), String> {
    use crate::managed_agents::{
        persona_events::monotonic_created_at,
        retention::{get_retained_event, open_retention_db, retain_event, RetainedEvent},
        waker_bundle::{sign_launch_bundle, BundleInputs, DEFAULT_BUNDLE_LIFETIME_SECS},
    };
    use buzz_core_pkg::kind::KIND_WAKER_BUNDLE_ENVELOPE;
    use nostr::{nips::nip44, JsonUtil, PublicKey, Tag};

    let bundle_version = ledger.reserve(agent_pubkey)?;

    let signed = sign_launch_bundle(
        BundleInputs {
            agent_pubkey: agent_pubkey.to_string(),
            agent_json,
            provider_id,
            provider_config,
            provider_binary_sha256,
            bundle_version,
            issued_at,
            lifetime_secs: DEFAULT_BUNDLE_LIFETIME_SECS,
            owner_only_access: crate::managed_agents::owner_only_access_build(),
            revoked,
        },
        owner_keys,
    )?;

    let plaintext = serde_json::to_string(&signed)
        .map_err(|e| format!("failed to serialize launch bundle: {e}"))?;
    let agent_pk =
        PublicKey::from_hex(agent_pubkey).map_err(|e| format!("invalid agent pubkey: {e}"))?;
    let ciphertext = nip44::encrypt(
        owner_keys.secret_key(),
        &agent_pk,
        &plaintext,
        nip44::Version::V2,
    )
    .map_err(|e| format!("failed to encrypt launch bundle: {e}"))?;

    let conn = open_retention_db(db_path)?;
    let owner_pubkey = owner_keys.public_key().to_hex();
    // The bump is for *this* store, not the relay: `retain_event` only
    // replaces a row when `excluded.created_at >= persona_events.created_at`,
    // and wall-clock seconds alone let a same-second issue-then-revoke (or two
    // reissues) tie — leaving the newer event silently dropped on the floor
    // and never queued for publish. Bumping past the retained head (the same
    // rule `reconcile::retain_agent_record` applies to the sibling 30177
    // record) guarantees the local row always advances.
    //
    // Relay-side ordering is deliberately not relied on here: the envelope is
    // not parameterized-replaceable, so nothing supersedes anything there —
    // the daemon's monotonic `FloorStore` is what makes ordering irrelevant.
    let existing = get_retained_event(
        &conn,
        KIND_WAKER_BUNDLE_ENVELOPE,
        &owner_pubkey,
        agent_pubkey,
    )?;

    // Published under the envelope kind, not the payload kind — see
    // `KIND_WAKER_BUNDLE_ENVELOPE`. The `d` tag is kept even though the
    // envelope is not parameterized-replaceable: it is what keys this row in
    // the retention store, and what `revoke_waker_bundle_pending` looks the
    // current bundle up by.
    let event = nostr::EventBuilder::new(
        nostr::Kind::Custom(KIND_WAKER_BUNDLE_ENVELOPE as u16),
        ciphertext,
    )
    .tags([
        Tag::parse(["d", agent_pubkey]).map_err(|e| e.to_string())?,
        Tag::parse(["p", agent_pubkey]).map_err(|e| e.to_string())?,
    ])
    .custom_created_at(monotonic_created_at(
        existing.as_ref().map(|row| row.created_at),
    ))
    .sign_with_keys(owner_keys)
    .map_err(|e| format!("failed to sign launch bundle event: {e}"))?;

    retain_event(
        &conn,
        &RetainedEvent {
            kind: KIND_WAKER_BUNDLE_ENVELOPE,
            pubkey: owner_pubkey,
            d_tag: agent_pubkey.to_string(),
            content: event.content.to_string(),
            created_at: event.created_at.as_secs() as i64,
            raw_event: event.as_json(),
            pending_sync: true,
        },
    )?;

    // Only now that a bundle is actually retained. A revocation clears the
    // entry instead of setting one: it leaves no bundle to lapse, so recording
    // a window for it would put a countdown on something already gone.
    //
    // Best-effort on purpose. This is the input to a warning, not to a
    // security decision — failing the whole reissue because a display hint
    // could not be written would trade a working bundle for a cosmetic one.
    let expiry = (!revoked).then(|| issued_at.saturating_add(DEFAULT_BUNDLE_LIFETIME_SECS));
    if let Err(error) = ledger.record_expiry(agent_pubkey, expiry) {
        eprintln!("waggle: could not record the waker bundle expiry: {error}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_agents::retention::{get_retained_event, open_retention_db};
    use crate::managed_agents::waker_bundle::IssuanceLedger;
    use buzz_core_pkg::kind::KIND_WAKER_BUNDLE_ENVELOPE;

    fn ledger(dir: &std::path::Path) -> IssuanceLedger {
        IssuanceLedger::open(dir.join("waker-bundle-versions.json"))
    }

    /// The retained row is what the desktop hands to the shared flush loop —
    /// this proves it round-trips through the exact decrypt+verify path
    /// `buzz-waker`'s own bundle tap runs (`bundle_feed::decrypt_verify_and_admit`),
    /// not a re-implementation of it.
    #[test]
    fn a_retained_bundle_decrypts_and_verifies_as_the_waker_would() {
        let dir = tempfile::tempdir().expect("tempdir");
        let owner = Keys::generate();
        let agent = Keys::generate();
        let agent_pubkey = agent.public_key().to_hex();

        sign_and_retain_waker_bundle_at(
            &dir.path().join("retention.sqlite"),
            &owner,
            &ledger(dir.path()),
            &agent_pubkey,
            serde_json::json!({"launch": {"command": "buzz-acp"}}),
            "sprites".to_string(),
            serde_json::json!({"org": "buzz-team"}),
            "b".repeat(64),
            1_000,
            false,
        )
        .expect("retain");

        let conn = open_retention_db(&dir.path().join("retention.sqlite")).expect("open db");
        let retained = get_retained_event(
            &conn,
            KIND_WAKER_BUNDLE_ENVELOPE,
            &owner.public_key().to_hex(),
            &agent_pubkey,
        )
        .expect("query")
        .expect("a launch bundle row was retained");
        assert!(retained.pending_sync, "must be queued for the flush loop");

        let plaintext =
            nostr::nips::nip44::decrypt(agent.secret_key(), &owner.public_key(), &retained.content)
                .expect("agent must be able to decrypt what the owner encrypted to it");
        let signed: buzz_waker_pkg::SignedLaunchBundle =
            serde_json::from_str(&plaintext).expect("parse signed bundle");
        let body = signed
            .verify(&owner.public_key().to_hex(), 2_000)
            .expect("the waker must accept what the desktop retained");
        assert_eq!(body.agent_pubkey, agent_pubkey);
        assert_eq!(body.provider.provider_id, "sprites");
        assert_eq!(body.bundle_version, 1);
    }

    /// A second retain for the same agent must never repeat a version — the
    /// whole point of `IssuanceLedger` (see `waker_bundle`'s own module doc):
    /// a repeated version lets `FloorStore::admit` treat it as a routine
    /// redelivery, leaving the first body's clamp/provider envelope
    /// replayable for its whole validity window.
    #[test]
    fn reissuing_for_the_same_agent_never_repeats_a_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let owner = Keys::generate();
        let agent = Keys::generate();
        let agent_pubkey = agent.public_key().to_hex();
        let ledger = ledger(dir.path());
        let db_path = dir.path().join("retention.sqlite");

        for issued_at in [1_000, 2_000] {
            sign_and_retain_waker_bundle_at(
                &db_path,
                &owner,
                &ledger,
                &agent_pubkey,
                serde_json::json!({}),
                "sprites".to_string(),
                serde_json::json!({}),
                "b".repeat(64),
                issued_at,
                false,
            )
            .expect("retain");
        }

        let conn = open_retention_db(&db_path).expect("open db");
        let retained = get_retained_event(
            &conn,
            KIND_WAKER_BUNDLE_ENVELOPE,
            &owner.public_key().to_hex(),
            &agent_pubkey,
        )
        .expect("query")
        .expect("a launch bundle row was retained");
        let plaintext =
            nostr::nips::nip44::decrypt(agent.secret_key(), &owner.public_key(), &retained.content)
                .expect("decrypt");
        let signed: buzz_waker_pkg::SignedLaunchBundle =
            serde_json::from_str(&plaintext).expect("parse");
        let body = signed
            .verify(&owner.public_key().to_hex(), 3_000)
            .expect("verify");
        assert_eq!(
            body.bundle_version, 2,
            "the second issuance for this agent must carry version 2, not repeat version 1"
        );
    }

    /// The headline case: a revocation supersedes the prior real bundle in
    /// *this* store, and what's left decrypts to a body the waker's own
    /// verifier accepts as `revoked`. This is what makes a revocation reach an
    /// already-connected daemon over the same live subscription a
    /// config-change reissue already uses.
    ///
    /// "Coordinate" here is the retention table's `(kind, pubkey, d_tag)`
    /// primary key, not a NIP-33 address: the envelope kind is not
    /// parameterized-replaceable, so on the relay the revocation lands beside
    /// the bundle it revokes rather than replacing it. That is safe for the
    /// reason [`revoke_waker_bundle_pending`] documents — the daemon's
    /// `FloorStore` refuses anything at or below the revoked version, durably
    /// and regardless of read order.
    #[test]
    fn a_revocation_replaces_the_retained_bundle_at_the_same_coordinate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let owner = Keys::generate();
        let agent = Keys::generate();
        let agent_pubkey = agent.public_key().to_hex();
        let ledger = ledger(dir.path());
        let db_path = dir.path().join("retention.sqlite");

        sign_and_retain_waker_bundle_at(
            &db_path,
            &owner,
            &ledger,
            &agent_pubkey,
            serde_json::json!({"launch": {"command": "buzz-acp"}}),
            "sprites".to_string(),
            serde_json::json!({"org": "buzz-team"}),
            "b".repeat(64),
            1_000,
            false,
        )
        .expect("issue the real bundle");

        sign_and_retain_waker_bundle_at(
            &db_path,
            &owner,
            &ledger,
            &agent_pubkey,
            serde_json::Value::Null,
            String::new(),
            serde_json::json!({}),
            String::new(),
            2_000,
            true,
        )
        .expect("revoke it");

        let conn = open_retention_db(&db_path).expect("open db");
        let retained = get_retained_event(
            &conn,
            KIND_WAKER_BUNDLE_ENVELOPE,
            &owner.public_key().to_hex(),
            &agent_pubkey,
        )
        .expect("query")
        .expect("exactly one row remains at the coordinate");
        assert!(retained.pending_sync, "must be queued for the flush loop");

        let plaintext =
            nostr::nips::nip44::decrypt(agent.secret_key(), &owner.public_key(), &retained.content)
                .expect("decrypt");
        let signed: buzz_waker_pkg::SignedLaunchBundle =
            serde_json::from_str(&plaintext).expect("parse signed bundle");
        let body = signed
            .verify(&owner.public_key().to_hex(), 3_000)
            .expect("a revocation must still verify");
        assert!(
            body.revoked,
            "the retained row must be the revocation, not the earlier real bundle"
        );
        assert_eq!(
            body.bundle_version, 2,
            "revoking still consumes a version from the same monotonic ledger"
        );
    }
}
