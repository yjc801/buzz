//! Starting a Remote-wake agent by asking `buzz-waker`, never by deploying it
//! from this machine.
//!
//! An agent with Remote wake on is deployed by the community's waker, which
//! holds the operator's substrate credential. Deploying it from here instead
//! would spend whatever credential this machine happens to hold — for a member
//! of a hosted community, none, so Start failed with a Sprites 401 and told the
//! member to mint a token they should never need. Start on such an agent now
//! publishes a signed start request (`buzz_waker_pkg::start_request`) and
//! returns; the agent comes online when the waker's deploy does, which is what
//! presence shows.

use buzz_core_pkg::kind::KIND_WAKER_BUNDLE_ENVELOPE;
use buzz_waker_pkg::{SignedStartRequest, StartRequestBody};
use nostr::nips::nip44;
use nostr::{EventBuilder, Keys, Kind, Tag};
use tauri::AppHandle;

use crate::app_state::AppState;
use crate::managed_agents::retention::RetentionScope;
use crate::managed_agents::{BackendKind, ManagedAgentRecord};

/// Same bound the retained-event flush uses for a one-shot WebSocket publish.
const START_REQUEST_PUBLISH_TIMEOUT_SECS: u64 = 20;

/// How a start of this record is carried out.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StartRoute {
    /// Spawn the harness on this machine.
    Local,
    /// Ask the community's `buzz-waker` to deploy it.
    Waker,
    /// Deploy it to its provider from this machine.
    Provider,
}

/// Decide how to start `record`. Remote wake wins over a direct provider
/// deploy: while it is on, the waker is the one that deploys this agent.
pub(crate) fn start_route(record: &ManagedAgentRecord) -> StartRoute {
    match record.backend {
        BackendKind::Local => StartRoute::Local,
        BackendKind::Provider { .. } if record.waker_enabled => StartRoute::Waker,
        BackendKind::Provider { .. } => StartRoute::Provider,
    }
}

/// Unix seconds, or a clear error if the clock is before the epoch.
pub(crate) fn now_unix() -> Result<u64, String> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("system clock before unix epoch: {e}"))?
        .as_secs())
}

/// Refuse a start the waker is known to refuse: it deploys only from an
/// unexpired launch bundle. An unknown expiry (`None`) is not refused — the
/// waker's own check is the authority, and this is only an early, clearer
/// error for the case the desktop can see.
pub(crate) fn ensure_bundle_live(
    agent_name: &str,
    bundle_expires_at: Option<u64>,
    now: u64,
) -> Result<(), String> {
    match bundle_expires_at {
        Some(expires_at) if expires_at <= now => Err(format!(
            "{agent_name}'s Remote wake launch bundle has expired, so the community waker \
             will not start it. Change any setting on this agent to reissue it, then try again."
        )),
        _ => Ok(()),
    }
}

/// Build the wire form of a start request: a gift wrap `#p`-tagged to the
/// agent, signed by a throwaway key, carrying the owner-signed request
/// NIP-44 encrypted to the agent. See `buzz_core::kind::KIND_WAKER_START_REQUEST`
/// for why the envelope is not owner-signed.
pub(crate) fn build_start_request_envelope(
    owner_keys: &Keys,
    agent_pubkey: &str,
    requested_at: u64,
) -> Result<nostr::Event, String> {
    let body = StartRequestBody {
        agent_pubkey: agent_pubkey.to_string(),
        requested_at,
    };
    let signed =
        SignedStartRequest::sign(&body, &owner_keys.secret_key().keypair(nostr::SECP256K1))
            .map_err(|e| format!("failed to sign start request: {e}"))?;
    let plaintext = serde_json::to_string(&signed)
        .map_err(|e| format!("failed to encode start request: {e}"))?;
    let agent = nostr::PublicKey::from_hex(agent_pubkey)
        .map_err(|e| format!("invalid agent pubkey {agent_pubkey}: {e}"))?;
    let throwaway = Keys::generate();
    let content = nip44::encrypt(
        throwaway.secret_key(),
        &agent,
        plaintext,
        nip44::Version::V2,
    )
    .map_err(|e| format!("failed to encrypt start request: {e}"))?;
    EventBuilder::new(Kind::Custom(KIND_WAKER_BUNDLE_ENVELOPE as u16), content)
        .tags([Tag::public_key(agent)])
        .sign_with_keys(&throwaway)
        .map_err(|e| format!("failed to sign start request envelope: {e}"))
}

/// Pin the retention scope a waker start runs in: the active `(relay, owner)`
/// and its database, resolved ONCE and refused unless it is the scope the
/// caller already validated.
///
/// Every step after this crosses an `.await`, and resolving the active scope
/// again at each step would let a community or identity switch in between
/// split them — flushing and checking community B's launch bundle while the
/// request goes to community A, whose bundle may never have left this machine.
/// [`request_waker_start_in`] keeps the returned scope for the flush, the
/// check and the publish alike.
fn pin_start_scope(
    app: &AppHandle,
    state: &AppState,
    expected_relay_url: &str,
    expected_owner_hex: &str,
) -> Result<RetentionScope, String> {
    ensure_start_scope(
        crate::managed_agents::retention::active_retention_scope(app, state)?,
        expected_relay_url,
        expected_owner_hex,
    )
}

/// [`pin_start_scope`]'s check, apart from the `AppHandle` it resolves with.
fn ensure_start_scope(
    scope: RetentionScope,
    expected_relay_url: &str,
    expected_owner_hex: &str,
) -> Result<RetentionScope, String> {
    let owner_matches = scope.owner_keys.public_key().to_hex() == expected_owner_hex;
    crate::managed_agents::retention::scope_for_arrival(scope, expected_relay_url)
        .filter(|_| owner_matches)
        .ok_or_else(|| {
            "the active community or identity changed while this agent was starting; \
             try again"
                .to_string()
        })
}

/// Ask the community waker to start `agent_pubkey`, in the scope the caller
/// validated: `expected_relay_url` and `expected_owner_hex`. Fails closed if
/// that is no longer the active scope; see [`pin_start_scope`].
pub(crate) async fn request_waker_start(
    app: &AppHandle,
    state: &AppState,
    agent_name: &str,
    agent_pubkey: &str,
    requested_at: u64,
    expected_relay_url: &str,
    expected_owner_hex: &str,
) -> Result<(), String> {
    let scope = pin_start_scope(app, state, expected_relay_url, expected_owner_hex)?;
    request_waker_start_in(state, &scope, agent_name, agent_pubkey, requested_at).await
}

/// Publish a start request for `agent_pubkey`, entirely within `scope`: its
/// relay, authenticated as its owner.
///
/// The agent's launch bundle is flushed first: it is retained locally and
/// published on a sweep, so a bundle issued moments ago (Remote wake just
/// turned on, or a setting just changed) may not be on the relay yet — and a
/// waker without it has nothing to deploy. A bundle still pending after the
/// flush fails the start rather than sending a request the waker cannot act
/// on. The flush, that check and the publish all use `scope` — never the
/// active one, which may have moved on by now (see [`pin_start_scope`]).
async fn request_waker_start_in(
    state: &AppState,
    scope: &RetentionScope,
    agent_name: &str,
    agent_pubkey: &str,
    requested_at: u64,
) -> Result<(), String> {
    if let Err(error) = crate::managed_agents::persona_events::flush_pending_events_at(
        &scope.db_path,
        state,
        &scope.relay_url,
        &scope.owner_keys,
    )
    .await
    {
        eprintln!("buzz-desktop: waker-start: pending-event flush failed: {error}");
    }
    if bundle_pending(scope, agent_pubkey)? {
        return Err(format!(
            "{agent_name}'s Remote wake launch bundle has not reached the relay yet, so the \
             community waker has nothing to start. Try again in a moment."
        ));
    }

    let event = build_start_request_envelope(&scope.owner_keys, agent_pubkey, requested_at)?;
    let ok = buzz_ws_client_pkg::publish_event(
        &scope.relay_url,
        event,
        &scope.owner_keys,
        None,
        START_REQUEST_PUBLISH_TIMEOUT_SECS,
    )
    .await
    .map_err(|e| format!("could not ask the community waker to start {agent_name}: {e}"))?;
    if !ok.accepted {
        return Err(format!(
            "the relay refused the request to start {agent_name}: {}",
            ok.message
        ));
    }
    Ok(())
}

/// Whether `agent_pubkey`'s launch bundle is still waiting to be published
/// from `scope`'s store.
fn bundle_pending(scope: &RetentionScope, agent_pubkey: &str) -> Result<bool, String> {
    use crate::managed_agents::retention::{get_retained_event, open_retention_db};
    let conn = open_retention_db(&scope.db_path)?;
    Ok(get_retained_event(
        &conn,
        KIND_WAKER_BUNDLE_ENVELOPE,
        &scope.owner_keys.public_key().to_hex(),
        agent_pubkey,
    )?
    .is_some_and(|event| event.pending_sync))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_record() -> ManagedAgentRecord {
        let mut record: ManagedAgentRecord = serde_json::from_value(serde_json::json!({
            "pubkey": "agent", "name": "Agent", "relay_url": "", "acp_command": "",
            "agent_command": "", "agent_args": [], "mcp_command": "",
            "turn_timeout_seconds": 0, "system_prompt": null, "created_at": "",
            "updated_at": "", "last_started_at": null, "last_stopped_at": null,
            "last_exit_code": null, "last_error": null
        }))
        .unwrap();
        record.backend = BackendKind::Provider {
            id: "sprites".into(),
            config: serde_json::json!({}),
        };
        record
    }

    #[test]
    fn remote_wake_routes_a_provider_start_to_the_waker() {
        let mut record = provider_record();
        record.waker_enabled = true;
        assert_eq!(start_route(&record), StartRoute::Waker);

        record.waker_enabled = false;
        assert_eq!(start_route(&record), StartRoute::Provider);

        record.backend = BackendKind::Local;
        assert_eq!(start_route(&record), StartRoute::Local);
    }

    #[test]
    fn an_expired_bundle_is_refused_before_anything_is_sent() {
        assert!(ensure_bundle_live("Chris", Some(100), 100).is_err());
        assert!(ensure_bundle_live("Chris", Some(101), 100).is_ok());
        assert!(ensure_bundle_live("Chris", None, 100).is_ok());
    }

    fn scope_at(dir: &std::path::Path, relay_url: &str, owner_keys: Keys) -> RetentionScope {
        RetentionScope {
            db_path: dir.join("retention.db"),
            relay_url: relay_url.to_string(),
            owner_keys,
        }
    }

    #[test]
    fn a_start_scope_other_than_the_validated_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let owner = Keys::generate();
        let owner_hex = owner.public_key().to_hex();
        let scope = || scope_at(dir.path(), "wss://a.example/", owner.clone());

        assert!(ensure_start_scope(scope(), "wss://a.example", &owner_hex).is_ok());
        assert!(ensure_start_scope(scope(), "wss://b.example", &owner_hex).is_err());
        let other_owner = Keys::generate().public_key().to_hex();
        assert!(ensure_start_scope(scope(), "wss://a.example", &other_owner).is_err());
    }

    /// Round 1's race: the pinned community's bundle flush fails, then the
    /// active community switches to one with nothing pending. The pending
    /// check must still read the PINNED store and refuse, not the active one.
    #[tokio::test]
    async fn a_bundle_still_pending_in_the_pinned_scope_refuses_after_a_switch() {
        use crate::managed_agents::retention::{open_retention_db, retain_event, RetainedEvent};
        use nostr::JsonUtil;

        let dir = tempfile::tempdir().unwrap();
        let (owner_a, agent) = (Keys::generate(), Keys::generate());
        let agent_hex = agent.public_key().to_hex();
        // Nothing listens here, so the pinned scope's flush fails.
        let scope_a = scope_at(dir.path(), "ws://127.0.0.1:1", owner_a.clone());
        let bundle = EventBuilder::new(Kind::Custom(KIND_WAKER_BUNDLE_ENVELOPE as u16), "sealed")
            .tags([Tag::parse(["d", agent_hex.as_str()]).unwrap()])
            .sign_with_keys(&owner_a)
            .unwrap();
        retain_event(
            &open_retention_db(&scope_a.db_path).unwrap(),
            &RetainedEvent {
                kind: KIND_WAKER_BUNDLE_ENVELOPE,
                pubkey: owner_a.public_key().to_hex(),
                d_tag: agent_hex.clone(),
                content: bundle.content.to_string(),
                created_at: bundle.created_at.as_secs() as i64,
                raw_event: bundle.as_json(),
                pending_sync: true,
            },
        )
        .unwrap();

        // The workspace has since switched to community B, identity B.
        let state = crate::app_state::build_app_state();
        *state.keys.lock().unwrap() = Keys::generate();
        *state.relay_url_override.lock().unwrap() = Some("ws://127.0.0.1:2".to_string());

        let error = request_waker_start_in(&state, &scope_a, "Chris", &agent_hex, 1_800_000_000)
            .await
            .unwrap_err();
        assert!(
            error.contains("has not reached the relay yet"),
            "unexpected error: {error}"
        );
        assert!(bundle_pending(&scope_a, &agent_hex).unwrap());
    }

    /// The envelope is exactly what the waker's tap accepts: the owner's
    /// signature inside, verifiable against the pinned owner, and an outer
    /// signer that is NOT the owner (so it stays out of the bundle query).
    #[test]
    fn the_envelope_is_one_the_waker_accepts() {
        let (owner, agent) = (Keys::generate(), Keys::generate());
        let agent_hex = agent.public_key().to_hex();
        let now = 1_800_000_000;
        let event = build_start_request_envelope(&owner, &agent_hex, now).unwrap();

        assert_eq!(event.kind.as_u16() as u32, KIND_WAKER_BUNDLE_ENVELOPE);
        assert_ne!(event.pubkey, owner.public_key());
        assert!(event
            .tags
            .iter()
            .any(|tag| tag.as_slice() == ["p".to_string(), agent_hex.clone()]));

        let plaintext = nip44::decrypt(agent.secret_key(), &event.pubkey, &event.content).unwrap();
        let signed: SignedStartRequest = serde_json::from_str(&plaintext).unwrap();
        let body = signed
            .verify(&owner.public_key().to_hex(), &agent_hex, now)
            .unwrap();
        assert_eq!(body.requested_at, now);
    }
}
