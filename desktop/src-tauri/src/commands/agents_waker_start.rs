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

/// Publish a start request for `agent_pubkey` to `relay_url`, authenticated as
/// the owner.
///
/// The agent's launch bundle is flushed first: it is retained locally and
/// published on a sweep, so a bundle issued moments ago (Remote wake just
/// turned on, or a setting just changed) may not be on the relay yet — and a
/// waker without it has nothing to deploy. A bundle still pending after the
/// flush fails the start rather than sending a request the waker cannot act
/// on.
pub(crate) async fn request_waker_start(
    app: &AppHandle,
    state: &AppState,
    agent_name: &str,
    agent_pubkey: &str,
    requested_at: u64,
    relay_url: &str,
    owner_keys: &Keys,
) -> Result<(), String> {
    if let Err(error) =
        crate::managed_agents::persona_events::flush_active_pending_events(app, state).await
    {
        eprintln!("buzz-desktop: waker-start: pending-event flush failed: {error}");
    }
    if crate::managed_agents::persona_events::active_pending_event(
        app,
        state,
        KIND_WAKER_BUNDLE_ENVELOPE,
        agent_pubkey,
    )? {
        return Err(format!(
            "{agent_name}'s Remote wake launch bundle has not reached the relay yet, so the \
             community waker has nothing to start. Try again in a moment."
        ));
    }

    let event = build_start_request_envelope(owner_keys, agent_pubkey, requested_at)?;
    let ok = buzz_ws_client_pkg::publish_event(
        relay_url,
        event,
        owner_keys,
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
