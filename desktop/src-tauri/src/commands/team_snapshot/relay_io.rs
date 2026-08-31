//! Relay- and retention-side I/O for team-snapshot import.
//!
//! Split out of `commands/team_snapshot.rs` to keep that file under the
//! desktop file-size ratchet. These two helpers are the only places the
//! import path talks to the retention database or posts to the relay
//! directly; the command bodies stay in the parent module.

use tauri::AppHandle;

use crate::{app_state::AppState, managed_agents::ManagedAgentRecord};

/// Inline retention for the managed-agent kind:30177 event — mirrors
/// `commands::personas::snapshot::import::retain_agent_pending`.
pub(super) fn retain_agent_pending(app: &AppHandle, state: &AppState, record: &ManagedAgentRecord) {
    use crate::managed_agents::{
        agent_events::{agent_event_content, build_agent_event},
        persona_events::monotonic_created_at,
        retention::{get_retained_event, open_retention_db, retain_event, RetainedEvent},
    };
    use buzz_core_pkg::kind::KIND_MANAGED_AGENT;
    use nostr::JsonUtil;

    let result = (|| -> Result<(), String> {
        let scope = crate::managed_agents::retention::active_retention_scope(app, state)?;
        let conn = open_retention_db(&scope.db_path)?;
        let content = serde_json::to_string(&agent_event_content(record))
            .map_err(|e| format!("failed to serialize agent content: {e}"))?;
        let (owner_pubkey, event) = {
            let keys = &scope.owner_keys;
            let owner_pubkey = keys.public_key().to_hex();
            let existing =
                get_retained_event(&conn, KIND_MANAGED_AGENT, &owner_pubkey, &record.pubkey)?;
            if existing.as_ref().is_some_and(|row| row.content == content) {
                return Ok(());
            }
            let event = build_agent_event(record)?
                .custom_created_at(monotonic_created_at(existing.map(|row| row.created_at)))
                .sign_with_keys(keys)
                .map_err(|e| format!("failed to sign agent event: {e}"))?;
            (owner_pubkey, event)
        };
        retain_event(
            &conn,
            &RetainedEvent {
                kind: KIND_MANAGED_AGENT,
                pubkey: owner_pubkey,
                d_tag: record.pubkey.clone(),
                content: event.content.to_string(),
                created_at: event.created_at.as_secs() as i64,
                raw_event: event.as_json(),
                pending_sync: true,
            },
        )
    })();
    if let Err(e) = result {
        eprintln!("buzz-desktop: team-snapshot-import retain-agent: {e}");
    }
}

/// POST a pre-built signed engram event to the relay, authenticating as the
/// new agent. Mirrors the same helper in `snapshot::import`.
pub(crate) async fn submit_engram_event(
    state: &AppState,
    agent_keys: &nostr::Keys,
    event_json: &[u8],
    url: &str,
    auth_tag: Option<&str>,
) -> Result<(), String> {
    use crate::relay::build_nip98_auth_header_for_keys;
    use reqwest::Method;

    crate::egress_guard::assert_no_key_backup_bytes(event_json, "team snapshot engram submit")?;

    // Wait before signing: the relay enforces NIP-98 freshness (±60s) and the
    // gate may hold for up to MAX_HINT_SECONDS (300s). Building auth before the
    // wait produces a stale `created_at` that the relay will reject.
    crate::relay_admission::wait_for_rate_limit().await;
    let auth = build_nip98_auth_header_for_keys(agent_keys, &Method::POST, url, event_json)?;
    let mut request = state
        .http_client
        .post(url)
        .header("Authorization", auth)
        .header("Content-Type", "application/json");
    if let Some(tag) = auth_tag {
        request = request.header("x-auth-tag", tag);
    }
    let response = request
        .body(event_json.to_vec())
        .send()
        .await
        .map_err(|e| crate::relay::classify_request_error(&e))?;

    if !response.status().is_success() {
        let msg = crate::relay::relay_error_message(response).await;
        return Err(format!("relay rejected engram: {msg}"));
    }

    let body = response
        .text()
        .await
        .map_err(|e| format!("failed to read relay response: {e}"))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("relay response not JSON: {e}"))?;
    let accepted = parsed
        .get("accepted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !accepted {
        let message = parsed
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return Err(format!("relay rejected engram: {message}"));
    }
    Ok(())
}
