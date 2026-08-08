use std::sync::atomic::Ordering;
use tauri::{AppHandle, Manager, State};

use crate::{
    app_state::AppState,
    managed_agents::{
        build_managed_agent_summary, current_instance_id, find_managed_agent_mut,
        load_managed_agents, load_personas, save_managed_agents, sync_managed_agent_processes,
        ManagedAgentSummary,
    },
    util::now_iso,
};

#[tauri::command]
pub fn set_agent_managed_profiles(enabled: bool, state: State<'_, AppState>) {
    state
        .managed_agent_profile_reconcile_enabled
        .store(!enabled, Ordering::Release);
}

#[tauri::command]
pub async fn set_managed_agent_start_on_app_launch(
    pubkey: String,
    start_on_app_launch: bool,
    app: AppHandle,
) -> Result<ManagedAgentSummary, String> {
    tokio::task::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let _store_guard = state
            .managed_agents_store_lock
            .lock()
            .map_err(|error| error.to_string())?;
        let mut records = load_managed_agents(&app)?;
        let mut runtimes = state
            .managed_agent_processes
            .lock()
            .map_err(|error| error.to_string())?;

        let (sync_changed, exited_pubkeys) =
            sync_managed_agent_processes(&mut records, &mut runtimes, &current_instance_id(&app));
        if sync_changed {
            save_managed_agents(&app, &records)?;
        }
        for pubkey in &exited_pubkeys {
            state.clear_agent_session_caches(pubkey);
        }

        {
            let record = find_managed_agent_mut(&mut records, &pubkey)?;
            record.start_on_app_launch = start_on_app_launch;
            record.updated_at = now_iso();
        }

        save_managed_agents(&app, &records)?;
        let record = records
            .iter()
            .find(|record| record.pubkey == pubkey)
            .ok_or_else(|| format!("agent {pubkey} not found"))?;
        let personas = load_personas(&app).unwrap_or_default();
        build_managed_agent_summary(
            &app,
            record,
            &runtimes,
            &personas,
            &crate::managed_agents::load_global_agent_config(&app).unwrap_or_default(),
        )
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
}

/// Assign a managed agent to a community (`Some`) or unscope it (`None` =
/// offered in every community). Display/uniqueness scope only — never
/// affects where the agent can run.
#[tauri::command]
pub async fn set_managed_agent_community(
    pubkey: String,
    community_relay_url: Option<String>,
    app: AppHandle,
) -> Result<ManagedAgentSummary, String> {
    // Unlike creation (which degrades to unscoped on a bad workspace relay),
    // an explicit assignment that fails to normalize is a hard error —
    // silently unscoping a value the user picked would be a lie.
    let community_relay_url = match community_relay_url {
        Some(url) => Some(
            buzz_core_pkg::relay::normalize_relay_url(&url)
                .map_err(|error| format!("invalid community relay URL: {error}"))?,
        ),
        None => None,
    };

    tokio::task::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let _store_guard = state
            .managed_agents_store_lock
            .lock()
            .map_err(|error| error.to_string())?;
        let mut records = load_managed_agents(&app)?;
        let mut runtimes = state
            .managed_agent_processes
            .lock()
            .map_err(|error| error.to_string())?;

        let (sync_changed, exited_pubkeys) =
            sync_managed_agent_processes(&mut records, &mut runtimes, &current_instance_id(&app));
        if sync_changed {
            save_managed_agents(&app, &records)?;
        }
        for pubkey in &exited_pubkeys {
            state.clear_agent_session_caches(pubkey);
        }

        {
            let record = find_managed_agent_mut(&mut records, &pubkey)?;
            let name = record.name.clone();
            // The same per-(community, name) rule as creation, against the
            // TARGET scope and excluding this record itself — assigning an
            // agent into a community that already offers that name would
            // manufacture exactly the ambiguity scoping removes.
            if crate::managed_agents::instance_name_taken_in_scope(
                &records,
                &name,
                community_relay_url.as_deref(),
                Some(&pubkey),
            ) {
                return Err(format!(
                    "an agent named \"{name}\" already exists in this community"
                ));
            }
            let record = find_managed_agent_mut(&mut records, &pubkey)?;
            record.community_relay_url = community_relay_url;
            record.updated_at = now_iso();
        }

        save_managed_agents(&app, &records)?;
        let record = records
            .iter()
            .find(|record| record.pubkey == pubkey)
            .ok_or_else(|| format!("agent {pubkey} not found"))?;
        let personas = load_personas(&app).unwrap_or_default();
        build_managed_agent_summary(
            &app,
            record,
            &runtimes,
            &personas,
            &crate::managed_agents::load_global_agent_config(&app).unwrap_or_default(),
        )
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
}

#[tauri::command]
pub async fn set_managed_agent_auto_restart(
    pubkey: String,
    auto_restart_on_config_change: bool,
    app: AppHandle,
) -> Result<ManagedAgentSummary, String> {
    tokio::task::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let _store_guard = state
            .managed_agents_store_lock
            .lock()
            .map_err(|error| error.to_string())?;
        let mut records = load_managed_agents(&app)?;
        let mut runtimes = state
            .managed_agent_processes
            .lock()
            .map_err(|error| error.to_string())?;

        let (sync_changed, exited_pubkeys) =
            sync_managed_agent_processes(&mut records, &mut runtimes, &current_instance_id(&app));
        if sync_changed {
            save_managed_agents(&app, &records)?;
        }
        for pubkey in &exited_pubkeys {
            state.clear_agent_session_caches(pubkey);
        }

        {
            let record = find_managed_agent_mut(&mut records, &pubkey)?;
            record.auto_restart_on_config_change = auto_restart_on_config_change;
            record.updated_at = now_iso();
        }

        save_managed_agents(&app, &records)?;
        let record = records
            .iter()
            .find(|record| record.pubkey == pubkey)
            .ok_or_else(|| format!("agent {pubkey} not found"))?;
        let personas = load_personas(&app).unwrap_or_default();
        build_managed_agent_summary(
            &app,
            record,
            &runtimes,
            &personas,
            &crate::managed_agents::load_global_agent_config(&app).unwrap_or_default(),
        )
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
}
