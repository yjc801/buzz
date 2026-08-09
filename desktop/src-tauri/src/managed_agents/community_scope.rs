//! Community-scope collision rules for managed-agent instances.
//!
//! An instance's `community_relay_url` names the community it belongs to;
//! `None` = unscoped, offered in every community. Scope is display and
//! name-uniqueness only — it never affects where an agent can run
//! (`effective_agent_relay_url`, #2122).

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
