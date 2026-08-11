//! Community-scope rules for managed-agent instances.
//!
//! An instance's `community_relay_url` names the community it belongs to;
//! `None` = unscoped, offered in every community. Scope was originally
//! display and name-uniqueness only (`effective_agent_relay_url`, #2122), but
//! it now also gates where the runtime may start: see
//! `home_community_allows`, added after off-home starts were found running
//! live and unstoppable from the app.

use super::runtime_types::ManagedAgentRuntimeKey;
use super::types::ManagedAgentRecord;

/// Whether two community scopes contend for the same picker namespace.
///
/// `None` (unscoped) is visible in every community, so it collides with
/// everything; two bound scopes collide only when equal. Both sides are
/// canonical relay URLs (`normalize_relay_url`) at write time, so string
/// equality is the right comparison.
pub fn community_scopes_collide(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (None, _) | (_, None) => true,
        (Some(left), Some(right)) => left == right,
    }
}

/// Whether an *instance* (keyed record) already holds `name` in the given
/// community scope. Case-insensitive — the point of the rule is
/// disambiguating an @-mention picker, where "Bumble" and "bumble" are
/// equally ambiguous. Definitions (key-less persona records) never collide:
/// a persona named Bumble must not block an instance named Bumble.
///
/// `exclude_pubkey` lets the assign flow re-check a record's own move
/// without colliding with itself.
pub fn instance_name_taken_in_scope(
    records: &[ManagedAgentRecord],
    name: &str,
    scope: Option<&str>,
    exclude_pubkey: Option<&str>,
) -> bool {
    let target = name.trim().to_lowercase();
    records.iter().any(|record| {
        !record.pubkey.is_empty()
            && exclude_pubkey.is_none_or(|pubkey| record.pubkey != pubkey)
            && record.name.trim().to_lowercase() == target
            && community_scopes_collide(record.community_relay_url.as_deref(), scope)
    })
}

/// The community a newly-minted instance belongs to: the active workspace
/// relay, canonicalized. `None` (unscoped) when the relay cannot be
/// canonicalized — creation must not fail on it; the record just degrades to
/// the pre-scoping behavior of being offered everywhere.
pub fn minted_community_scope(workspace_relay: &str) -> Option<String> {
    buzz_core_pkg::relay::normalize_relay_url(workspace_relay).ok()
}

/// Mint-time gate for agent creation: resolves the new instance's community
/// scope from the active workspace relay and enforces per-(community, name)
/// uniqueness against the loaded store. Must be called inside the
/// managed-agents store lock — the same critical section as the write — so
/// two concurrent creates cannot both pass. Create-time only: pre-existing
/// duplicates in the store keep working.
pub fn mint_scope_and_check_name(
    records: &[ManagedAgentRecord],
    name: &str,
    workspace_relay: &str,
) -> Result<Option<String>, String> {
    let scope = minted_community_scope(workspace_relay);
    if instance_name_taken_in_scope(records, name, scope.as_deref(), None) {
        return Err(format!(
            "an agent named \"{name}\" already exists in this community"
        ));
    }
    Ok(scope)
}

/// Refuse to start an agent on a relay that is not its assigned community.
///
/// The runtime is keyed on `(pubkey, relay_url)`, so the same identity can hold
/// a separate harness per relay. Assigning `community_relay_url` scopes which
/// community *renders* an agent, but on its own it does not scope the runtime:
/// connecting to another community would start a second harness for the same
/// identity on a relay where that agent has no row in the UI. The result is an
/// agent that is live and answering in a community that insists it does not
/// exist there — invisible, and unstoppable from the app, because stopping is
/// driven from the row that was never drawn.
///
/// Called from `spawn_agent_child`, the shared boundary every start path
/// (interactive start, launch restore, and reconcile-on-launch) funnels
/// through — so no caller can bypass it by reaching one of those paths
/// directly instead of going through `start_pair`.
///
/// An agent with no `community_relay_url` is unassigned and may run anywhere,
/// which is the pre-assignment behaviour and what keeps this backward
/// compatible.
pub fn home_community_allows(
    record: &ManagedAgentRecord,
    key: &ManagedAgentRuntimeKey,
) -> Result<(), String> {
    let Some(home) = record.community_relay_url.as_deref() else {
        return Ok(());
    };
    // Normalize both sides: the stored value is whatever was assigned, while
    // the key is already canonical, so a trailing slash alone must not read as
    // a different community.
    let home =
        buzz_core_pkg::relay::normalize_relay_url(home).map_err(|error| error.to_string())?;
    if home == key.relay_url {
        return Ok(());
    }
    Err(format!(
        "{} belongs to {home} and will not be started on {}",
        record.name, key.relay_url
    ))
}

#[cfg(test)]
mod home_community_allows_tests {
    use super::*;

    const HOME: &str = "wss://openvelvet.communities.buzz.xyz";
    const OTHER: &str = "wss://devenish.communities.buzz.xyz";

    fn record_with_home(home: Option<&str>) -> ManagedAgentRecord {
        let mut record: ManagedAgentRecord = serde_json::from_str(&format!(
            r#"{{
                "pubkey": "{}",
                "name": "Will",
                "relay_url": "{HOME}",
                "acp_command": "buzz-acp",
                "agent_command": "goose",
                "agent_args": [],
                "mcp_command": "",
                "turn_timeout_seconds": 320,
                "system_prompt": "",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z"
            }}"#,
            "aa".repeat(32)
        ))
        .unwrap();
        record.community_relay_url = home.map(str::to_owned);
        record
    }

    fn key_on(relay_url: &str) -> ManagedAgentRuntimeKey {
        ManagedAgentRuntimeKey::new("aa".repeat(32), relay_url).expect("valid key")
    }

    #[test]
    fn an_unassigned_agent_may_run_on_any_relay() {
        // Pre-assignment behaviour: no home means no restriction. Tightening
        // this would strand every agent created before community scoping.
        let record = record_with_home(None);
        assert!(home_community_allows(&record, &key_on(HOME)).is_ok());
        assert!(home_community_allows(&record, &key_on(OTHER)).is_ok());
    }

    #[test]
    fn an_assigned_agent_may_run_on_its_own_community() {
        let record = record_with_home(Some(HOME));
        assert!(home_community_allows(&record, &key_on(HOME)).is_ok());
    }

    #[test]
    fn an_assigned_agent_is_refused_on_another_community() {
        // The regression: connecting to another community used to start a
        // second harness for this identity, with no row in that community's UI
        // to stop it from.
        let record = record_with_home(Some(HOME));
        let error = home_community_allows(&record, &key_on(OTHER))
            .expect_err("must refuse a relay that is not the agent's home");
        // The message has to name both ends — "cannot start" alone leaves you
        // guessing which community is the blocker, which is the failure mode
        // that made this expensive to diagnose in the first place.
        assert!(error.contains("Will"), "{error}");
        assert!(error.contains("openvelvet"), "{error}");
        assert!(error.contains("devenish"), "{error}");
    }

    #[test]
    fn a_trailing_slash_is_not_a_different_community() {
        // The stored home is whatever was assigned; the key is already
        // canonical. Comparing raw strings would refuse an agent from its own
        // community over punctuation.
        let record = record_with_home(Some(&format!("{HOME}/")));
        assert!(home_community_allows(&record, &key_on(HOME)).is_ok());
    }

    #[test]
    fn an_unparseable_home_is_refused_rather_than_ignored() {
        // Fail closed: a home we cannot normalize must not silently widen into
        // "runs anywhere", which is the exact bug this guard exists to stop.
        let record = record_with_home(Some("not a relay url"));
        assert!(home_community_allows(&record, &key_on(HOME)).is_err());
    }
}
