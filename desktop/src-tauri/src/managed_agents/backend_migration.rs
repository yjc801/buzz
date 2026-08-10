//! Preconditions for moving an existing agent between local and provider
//! backends, without changing its identity.
//!
//! Extracted from the `set_managed_agent_backend` command so the rules are
//! testable without an `AppHandle`: the command supplies observed state, this
//! decides whether the move is allowed.
//!
//! The invariant these rules protect is **one identity, one live harness**.
//! Two harnesses signing as the same pubkey means doubled replies, flapping
//! presence, and concurrent NIP-AE writes against one `(agent, owner)` pair.

use super::types::BackendKind;

/// Observed state a migration decision depends on. Grouped into a struct
/// because three of the four are booleans — positional `bool` arguments at a
/// call site are exactly how a guard silently inverts.
pub struct MigrationPreconditions {
    /// Whether a local process for this agent is still alive, as reconciled by
    /// `sync_managed_agent_processes`. Authoritative for local agents.
    pub local_process_alive: bool,
    /// The caller's assertion, made against relay presence, that no remote
    /// harness is running. Only consulted when leaving a provider backend —
    /// see [`validate_backend_migration`] for why it cannot be verified here.
    pub remote_confirmed_stopped: bool,
    /// Whether this agent is pinned to Buzz shared compute (`relay_mesh`).
    pub uses_relay_mesh: bool,
}

/// Decide whether `current` → `target` is allowed.
///
/// Liveness is only observable for local agents. `build_managed_agent_summary`
/// reports provider agents as `deployed`/`not_deployed` from
/// `backend_agent_id`, which is *infrastructure existence* — a sprite stays
/// "deployed" after `!shutdown`. The real signal is relay presence, which the
/// frontend polls and the backend never sees. So leaving a provider requires
/// the caller to assert stoppedness rather than this function checking it.
///
/// Ordering matters: cheap identity checks first, then the two that produce
/// actionable instructions, so a user who is both running *and* on shared
/// compute is told the thing they can actually fix.
pub fn validate_backend_migration(
    current: &BackendKind,
    target: &BackendKind,
    observed: &MigrationPreconditions,
) -> Result<(), String> {
    if current == target {
        return Err("this agent already runs there".to_string());
    }

    // Shared-compute agents run against a relay-hosted model that a provider
    // deployment cannot reach. Creation enforces the same rule in
    // `normalize_relay_mesh`; this is the migration-time half of it.
    if observed.uses_relay_mesh && *target != BackendKind::Local {
        return Err("Buzz shared compute agents must use the local backend".to_string());
    }

    if observed.local_process_alive {
        return Err("stop the agent before moving it".to_string());
    }

    let leaving_provider = *current != BackendKind::Local;
    if leaving_provider && !observed.remote_confirmed_stopped {
        return Err(
            "send `!shutdown` and wait for the agent to go offline before moving it back — \
             a deployment that is still running would keep answering as this agent"
                .to_string(),
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str) -> BackendKind {
        BackendKind::Provider {
            id: id.to_string(),
            config: serde_json::json!({}),
        }
    }

    /// Everything observed is quiet — the shape of a legal migration.
    fn idle() -> MigrationPreconditions {
        MigrationPreconditions {
            local_process_alive: false,
            remote_confirmed_stopped: false,
            uses_relay_mesh: false,
        }
    }

    #[test]
    fn local_to_provider_is_allowed_when_stopped() {
        assert!(
            validate_backend_migration(&BackendKind::Local, &provider("sprites"), &idle()).is_ok()
        );
    }

    #[test]
    fn a_running_local_process_blocks_both_directions() {
        let observed = MigrationPreconditions {
            local_process_alive: true,
            remote_confirmed_stopped: true,
            ..idle()
        };
        assert_eq!(
            validate_backend_migration(&BackendKind::Local, &provider("sprites"), &observed)
                .unwrap_err(),
            "stop the agent before moving it"
        );
        assert_eq!(
            validate_backend_migration(&provider("sprites"), &BackendKind::Local, &observed)
                .unwrap_err(),
            "stop the agent before moving it"
        );
    }

    /// The asymmetry that motivates the whole module: the backend cannot see a
    /// remote harness, so leaving a provider needs the caller's assertion.
    #[test]
    fn leaving_a_provider_requires_the_stopped_assertion() {
        let error =
            validate_backend_migration(&provider("sprites"), &BackendKind::Local, &idle())
                .unwrap_err();
        assert!(error.contains("!shutdown"), "{error}");

        let confirmed = MigrationPreconditions {
            remote_confirmed_stopped: true,
            ..idle()
        };
        assert!(
            validate_backend_migration(&provider("sprites"), &BackendKind::Local, &confirmed)
                .is_ok()
        );
    }

    /// Entering a provider must NOT demand the assertion — there is no remote
    /// harness yet, and requiring it would make the common direction unusable.
    #[test]
    fn entering_a_provider_ignores_the_stopped_assertion() {
        assert!(
            validate_backend_migration(&BackendKind::Local, &provider("sprites"), &idle()).is_ok()
        );
    }

    #[test]
    fn relay_mesh_agents_cannot_move_to_a_provider() {
        let observed = MigrationPreconditions {
            uses_relay_mesh: true,
            ..idle()
        };
        assert_eq!(
            validate_backend_migration(&BackendKind::Local, &provider("sprites"), &observed)
                .unwrap_err(),
            "Buzz shared compute agents must use the local backend"
        );
    }

    /// ...but may still move back, which is the direction that unbreaks an
    /// agent someone already pushed onto a provider.
    #[test]
    fn relay_mesh_agents_may_return_to_local() {
        let observed = MigrationPreconditions {
            uses_relay_mesh: true,
            remote_confirmed_stopped: true,
            ..idle()
        };
        assert!(
            validate_backend_migration(&provider("sprites"), &BackendKind::Local, &observed).is_ok()
        );
    }

    #[test]
    fn a_no_op_is_rejected_before_anything_else() {
        // Even with every other precondition failing, sameness wins — the user
        // gets "already runs there" rather than a stop instruction for a move
        // that would do nothing.
        let observed = MigrationPreconditions {
            local_process_alive: true,
            remote_confirmed_stopped: false,
            uses_relay_mesh: true,
        };
        assert_eq!(
            validate_backend_migration(&BackendKind::Local, &BackendKind::Local, &observed)
                .unwrap_err(),
            "this agent already runs there"
        );
    }

    /// Same provider id but a different config is a real change (a re-deploy
    /// with new settings), not a no-op — `BackendKind` compares config too.
    #[test]
    fn same_provider_with_different_config_is_not_a_no_op() {
        let current = BackendKind::Provider {
            id: "sprites".to_string(),
            config: serde_json::json!({"inactivity_seconds": 7200}),
        };
        let target = BackendKind::Provider {
            id: "sprites".to_string(),
            config: serde_json::json!({"inactivity_seconds": 600}),
        };
        let observed = MigrationPreconditions {
            remote_confirmed_stopped: true,
            ..idle()
        };
        assert!(validate_backend_migration(&current, &target, &observed).is_ok());
    }
}
