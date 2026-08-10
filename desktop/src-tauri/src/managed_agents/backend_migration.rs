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

use std::collections::HashSet;
use std::sync::Mutex;

use super::types::{BackendKind, ResidualDeployment};

/// Observed state a migration decision depends on. Grouped into a struct
/// because three of the four are booleans — positional `bool` arguments at a
/// call site are exactly how a guard silently inverts.
pub struct MigrationPreconditions {
    /// Whether a local harness for this agent is still alive.
    ///
    /// Must come from the runtime map (every tracked pair for this pubkey, in
    /// every community) and the on-disk pair receipts — **not** from
    /// `record.runtime_pid`, which `sync_managed_agent_processes`
    /// unconditionally clears as legacy bookkeeping before any caller of this
    /// function gets to read it. See `local_harness_alive` in the command.
    pub local_harness_alive: bool,
    /// The caller's assertion, made against relay presence, that no remote
    /// harness is running. Only consulted when leaving a provider backend —
    /// see [`validate_backend_migration`] for why it cannot be verified here.
    pub remote_confirmed_stopped: bool,
    /// Whether this agent is pinned to Buzz shared compute.
    ///
    /// Must be resolved through `resolve_effective_relay_mesh_model_id` —
    /// `record.relay_mesh` is a backward-compatibility marker, not the source
    /// of truth, and is stale for linked instances and for agents inheriting
    /// the global default (see the field's own doc comment on
    /// `ManagedAgentRecord`).
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

    if observed.local_harness_alive {
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

/// Move the live deployment pointer into the residual list when a migration
/// takes the agent off the provider that owns it.
///
/// `backend_agent_id` names a deployment on the *current* backend. Keeping it
/// across a backend change (the original design here) preserves the id but
/// destroys its attribution, and both consumers depend on that attribution:
/// `delete_managed_agent` guards on `backend != Local`, so a Local record with
/// a naked id deletes silently and orphans infrastructure that still holds
/// this agent's private key; and on Provider A → Provider B, A's id makes B
/// read as already `deployed` until B's first deploy overwrites the only
/// pointer to A.
///
/// So the pointer moves rather than lingering: retired into
/// `residual_deployments` with the provider *and the config* that issued it,
/// and cleared from `backend_agent_id`.
///
/// Changing the config on the same provider is a retirement too. An earlier
/// revision returned early whenever the provider ids matched, on the reasoning
/// that a same-provider edit re-deploys the one live deployment. That holds
/// only while the config does not name a different deployment scope — and
/// `provider_config` is exactly where the Kubernetes provider's `context` and
/// `namespace` live, so a namespace edit strands the old pod while the record
/// keeps pointing at the new one (see [`ResidualDeployment`]). The desktop
/// cannot distinguish that from a harmless tuning change, so every config
/// change retires.
pub fn retire_deployment_pointer(
    current: &BackendKind,
    target: &BackendKind,
    backend_agent_id: &mut Option<String>,
    residual: &mut Vec<ResidualDeployment>,
) {
    let BackendKind::Provider {
        id: current_id,
        config: current_config,
    } = current
    else {
        return;
    };
    if let BackendKind::Provider {
        id: target_id,
        config: target_config,
    } = target
    {
        // Same provider *and* same scope is not a move at all —
        // `validate_backend_migration` rejects it first. Kept so this stays
        // total rather than relying on that ordering.
        if target_id == current_id && target_config == current_config {
            return;
        }
    }
    let Some(agent_id) = backend_agent_id.take() else {
        return;
    };
    let entry = ResidualDeployment {
        provider_id: current_id.clone(),
        agent_id,
        config: current_config.clone(),
    };
    if !residual.contains(&entry) {
        residual.push(entry);
    }
}

/// Drop a residual entry that a fresh deploy has just taken over.
///
/// A provider may issue a *deterministic* id — sprites derives it from the
/// agent, Kubernetes names the pod from it — so moving A → Local → A and
/// redeploying returns the exact id that [`retire_deployment_pointer`] retired
/// on the way out. Without this the same deployment is recorded as both current
/// (`backend_agent_id`) and abandoned (`residual_deployments`), and deletion
/// warns about orphaning infrastructure the agent is in fact still using.
///
/// Matching is exact on **all three** parts, and the config is the load-bearing
/// one. Determinism is what makes ids repeat, and it repeats *per scope*: the
/// same pod name exists independently in every namespace. Matching on
/// `(provider_id, agent_id)` alone would let a redeploy into namespace B
/// discard the residual naming the pod in namespace A — which still exists and
/// still holds the key. A different id on the same provider, or the same id on
/// a different provider, is likewise other infrastructure and stays.
pub fn reclaim_residual_deployment(
    provider_id: &str,
    config: &serde_json::Value,
    agent_id: &str,
    residual: &mut Vec<ResidualDeployment>,
) {
    residual.retain(|entry| {
        !(entry.provider_id == provider_id && entry.agent_id == agent_id && &entry.config == config)
    });
}

/// Whether deleting this agent would orphan provider infrastructure that still
/// holds a copy of its private key — the condition `delete_managed_agent`
/// refuses without `force_remote_delete`.
///
/// Two ways to qualify, and the second is the one a backend-only check misses:
/// the agent is on a provider and has been deployed there, **or** it has moved
/// off a provider that still has its deployment. In the second case `backend`
/// reads `Local`, which is exactly why the guard cannot be written in terms of
/// `backend` alone.
pub fn deletion_orphans_infrastructure(
    backend: &BackendKind,
    backend_agent_id: Option<&str>,
    residual: &[ResidualDeployment],
) -> bool {
    (*backend != BackendKind::Local && backend_agent_id.is_some()) || !residual.is_empty()
}

impl super::types::ManagedAgentRecord {
    /// [`deletion_orphans_infrastructure`] applied to this record.
    pub fn orphans_infrastructure(&self) -> bool {
        deletion_orphans_infrastructure(
            &self.backend,
            self.backend_agent_id.as_deref(),
            &self.residual_deployments,
        )
    }
}

/// Exclusive claim on an agent's backend for the duration of an operation that
/// can change where it runs, or that acts on where it runs while releasing the
/// store lock partway through.
///
/// The store lock is not enough on its own. `start_managed_agent` reads the
/// backend under the lock, releases it, spends up to the provider's deploy
/// timeout in an external process, then reacquires the lock and writes
/// `backend_agent_id`. A migration that lands inside that window is durable
/// before the deploy finishes, so the deploy goes on to start a remote harness
/// for a record that now says Local — and Local then permits a second, local
/// harness for the same key. A check after the deploy cannot help: the
/// external effect has already happened.
///
/// The fence therefore spans the whole operation including the external call.
/// It is always taken **before** the managed-agents store lock, never while
/// holding it, so the two locks have a fixed order and cannot deadlock.
///
/// Process-global rather than a field on `AppState`, matching `PATH_MUTEX` in
/// this module's parent: an agent identity is global, so a move must exclude a
/// deploy no matter which community either was initiated from.
pub struct BackendTransitionGuard<'a> {
    transitions: &'a Mutex<Option<HashSet<String>>>,
    pubkey: String,
}

impl Drop for BackendTransitionGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut held) = self.transitions.lock() {
            if let Some(set) = held.as_mut() {
                set.remove(&self.pubkey);
            }
        }
    }
}

static BACKEND_TRANSITIONS: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Claim the backend-transition fence for `pubkey`, or fail if another
/// operation already holds it.
///
/// Non-blocking on purpose: the operations it fences take seconds to minutes,
/// and a Tauri command that silently waits that long is indistinguishable from
/// a hang. Telling the caller to retry is the honest outcome.
pub fn begin_backend_transition(pubkey: &str) -> Result<BackendTransitionGuard<'static>, String> {
    begin_backend_transition_in(&BACKEND_TRANSITIONS, pubkey)
}

/// Injectable form of [`begin_backend_transition`] so the fence's exclusion and
/// release behaviour can be tested without touching the process-global set.
pub(crate) fn begin_backend_transition_in<'a>(
    transitions: &'a Mutex<Option<HashSet<String>>>,
    pubkey: &str,
) -> Result<BackendTransitionGuard<'a>, String> {
    let mut held = transitions.lock().map_err(|error| error.to_string())?;
    if !held
        .get_or_insert_with(HashSet::new)
        .insert(pubkey.to_string())
    {
        return Err(
            "this agent is already being started or moved — wait for that to finish".to_string(),
        );
    }
    drop(held);
    Ok(BackendTransitionGuard {
        transitions,
        pubkey: pubkey.to_string(),
    })
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
            local_harness_alive: false,
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
            local_harness_alive: true,
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
        let error = validate_backend_migration(&provider("sprites"), &BackendKind::Local, &idle())
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
            validate_backend_migration(&provider("sprites"), &BackendKind::Local, &observed)
                .is_ok()
        );
    }

    #[test]
    fn a_no_op_is_rejected_before_anything_else() {
        // Even with every other precondition failing, sameness wins — the user
        // gets "already runs there" rather than a stop instruction for a move
        // that would do nothing.
        let observed = MigrationPreconditions {
            local_harness_alive: true,
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

    // ── retire_deployment_pointer ────────────────────────────────────────

    fn residual(provider_id: &str, agent_id: &str) -> ResidualDeployment {
        scoped_residual(provider_id, agent_id, serde_json::json!({}))
    }

    fn scoped_residual(
        provider_id: &str,
        agent_id: &str,
        config: serde_json::Value,
    ) -> ResidualDeployment {
        ResidualDeployment {
            provider_id: provider_id.to_string(),
            agent_id: agent_id.to_string(),
            config,
        }
    }

    /// A Kubernetes-shaped backend: the scope lives in `provider_config`.
    fn in_namespace(namespace: &str) -> BackendKind {
        BackendKind::Provider {
            id: "kubernetes".to_string(),
            config: serde_json::json!({"context": "prod", "namespace": namespace}),
        }
    }

    /// The case that made a Local record delete without the remote-delete
    /// guard: the id must stop being a naked scalar and become attributable.
    #[test]
    fn leaving_a_provider_for_local_retires_the_pointer() {
        let mut backend_agent_id = Some("sprite-1".to_string());
        let mut residuals = Vec::new();
        retire_deployment_pointer(
            &provider("sprites"),
            &BackendKind::Local,
            &mut backend_agent_id,
            &mut residuals,
        );
        assert_eq!(backend_agent_id, None);
        assert_eq!(residuals, vec![residual("sprites", "sprite-1")]);
    }

    /// Provider A → Provider B: A's id must not stay behind to make B read as
    /// `deployed`, and B's first deploy must not be able to overwrite it.
    #[test]
    fn switching_providers_retires_the_old_pointer() {
        let mut backend_agent_id = Some("sprite-1".to_string());
        let mut residuals = Vec::new();
        retire_deployment_pointer(
            &provider("sprites"),
            &provider("blox"),
            &mut backend_agent_id,
            &mut residuals,
        );
        assert_eq!(backend_agent_id, None);
        assert_eq!(residuals, vec![residual("sprites", "sprite-1")]);
    }

    /// A same-provider config edit retires too, because the config is where
    /// the deployment scope lives. Editing the namespace strands the pod in the
    /// old one; the desktop cannot tell that from an `inactivity_seconds` tweak,
    /// so it keeps the pointer either way rather than guessing.
    #[test]
    fn a_same_provider_config_change_retires_the_old_scope() {
        let current = in_namespace("team-a");
        let target = in_namespace("team-b");
        let mut backend_agent_id = Some("pod-x".to_string());
        let mut residuals = Vec::new();
        retire_deployment_pointer(&current, &target, &mut backend_agent_id, &mut residuals);
        assert_eq!(backend_agent_id, None);
        assert_eq!(
            residuals,
            vec![scoped_residual(
                "kubernetes",
                "pod-x",
                serde_json::json!({"context": "prod", "namespace": "team-a"}),
            )],
            "the pod in team-a still exists and still holds the key"
        );
    }

    /// Same provider *and* same config is not a move at all —
    /// `validate_backend_migration` refuses it first, and this stays total.
    #[test]
    fn an_unchanged_backend_retires_nothing() {
        let mut backend_agent_id = Some("pod-x".to_string());
        let mut residuals = Vec::new();
        retire_deployment_pointer(
            &in_namespace("team-a"),
            &in_namespace("team-a"),
            &mut backend_agent_id,
            &mut residuals,
        );
        assert_eq!(backend_agent_id.as_deref(), Some("pod-x"));
        assert!(residuals.is_empty());
    }

    #[test]
    fn a_provider_that_was_never_deployed_leaves_nothing_behind() {
        let mut backend_agent_id = None;
        let mut residuals = Vec::new();
        retire_deployment_pointer(
            &provider("sprites"),
            &BackendKind::Local,
            &mut backend_agent_id,
            &mut residuals,
        );
        assert!(residuals.is_empty());
    }

    #[test]
    fn leaving_local_has_no_pointer_to_retire() {
        let mut backend_agent_id = None;
        let mut residuals = vec![residual("sprites", "sprite-1")];
        retire_deployment_pointer(
            &BackendKind::Local,
            &provider("blox"),
            &mut backend_agent_id,
            &mut residuals,
        );
        assert_eq!(backend_agent_id, None);
        // An earlier residual survives the move — it still exists and still
        // holds the key, whatever the agent does next.
        assert_eq!(residuals, vec![residual("sprites", "sprite-1")]);
    }

    /// Local → sprites → Local → sprites → Local must not accumulate the same
    /// deployment twice; the list names distinct infrastructure.
    #[test]
    fn retiring_the_same_deployment_twice_does_not_duplicate_it() {
        let mut residuals = vec![residual("sprites", "sprite-1")];
        let mut backend_agent_id = Some("sprite-1".to_string());
        retire_deployment_pointer(
            &provider("sprites"),
            &BackendKind::Local,
            &mut backend_agent_id,
            &mut residuals,
        );
        assert_eq!(residuals, vec![residual("sprites", "sprite-1")]);
    }

    // ── reclaim_residual_deployment ──────────────────────────────────────

    /// The round trip that produces a double-counted deployment: sprites hands
    /// back the id it issued before, so the entry retired on the way out names
    /// the deployment the agent is using again.
    #[test]
    fn redeploying_onto_a_retired_deployment_reclaims_it() {
        let mut residuals = vec![residual("sprites", "sprite-1")];
        reclaim_residual_deployment(
            "sprites",
            &serde_json::json!({}),
            "sprite-1",
            &mut residuals,
        );
        assert!(residuals.is_empty());
    }

    /// The scope regression. Deterministic naming repeats the pod name in every
    /// namespace, so `(provider, agent_id)` alone would let a deploy into
    /// team-b discard the residual naming the pod in team-a — which still
    /// exists, and still has the key.
    #[test]
    fn a_redeploy_into_another_scope_does_not_reclaim_the_old_one() {
        let team_a = serde_json::json!({"context": "prod", "namespace": "team-a"});
        let team_b = serde_json::json!({"context": "prod", "namespace": "team-b"});
        let mut residuals = vec![scoped_residual("kubernetes", "pod-x", team_a.clone())];

        reclaim_residual_deployment("kubernetes", &team_b, "pod-x", &mut residuals);

        assert_eq!(
            residuals,
            vec![scoped_residual("kubernetes", "pod-x", team_a)],
            "same provider and same deterministic id, different namespace"
        );
    }

    /// A different cluster is a different scope even with the same namespace.
    #[test]
    fn a_redeploy_against_another_context_does_not_reclaim_either() {
        let prod = serde_json::json!({"context": "prod", "namespace": "team-a"});
        let staging = serde_json::json!({"context": "staging", "namespace": "team-a"});
        let mut residuals = vec![scoped_residual("kubernetes", "pod-x", prod.clone())];

        reclaim_residual_deployment("kubernetes", &staging, "pod-x", &mut residuals);

        assert_eq!(
            residuals,
            vec![scoped_residual("kubernetes", "pod-x", prod)]
        );
    }

    /// Returning to the *same* scope still reclaims — the round-2 fix survives
    /// scope-qualification, and no false orphan warning comes back.
    #[test]
    fn returning_to_the_same_scope_still_reclaims() {
        let team_a = serde_json::json!({"context": "prod", "namespace": "team-a"});
        let mut residuals = vec![scoped_residual("kubernetes", "pod-x", team_a.clone())];

        reclaim_residual_deployment("kubernetes", &team_a, "pod-x", &mut residuals);

        assert!(residuals.is_empty());
        // Deletion still warns — the agent is deployed right now — but the
        // warning has one source, not a second one claiming it also abandoned
        // infrastructure it is actively using.
        assert!(deletion_orphans_infrastructure(
            &in_namespace("team-a"),
            Some("pod-x"),
            &residuals,
        ));
        assert!(!deletion_orphans_infrastructure(
            &BackendKind::Local,
            None,
            &residuals,
        ));
    }

    /// A residual written before residuals carried scope has an empty config.
    /// It must not be reclaimed by a deploy carrying real config — the safe
    /// direction is to keep warning about a deployment we cannot place.
    #[test]
    fn a_scopeless_legacy_residual_is_not_reclaimed_by_a_scoped_deploy() {
        let mut residuals = vec![residual("kubernetes", "pod-x")];
        reclaim_residual_deployment(
            "kubernetes",
            &serde_json::json!({"namespace": "team-a"}),
            "pod-x",
            &mut residuals,
        );
        assert_eq!(residuals, vec![residual("kubernetes", "pod-x")]);
    }

    /// A new id on the same provider is different infrastructure — the old one
    /// still exists, still holds the key, and must keep blocking deletion.
    #[test]
    fn a_new_id_on_the_same_provider_leaves_the_old_residual_alone() {
        let mut residuals = vec![residual("sprites", "sprite-1")];
        reclaim_residual_deployment(
            "sprites",
            &serde_json::json!({}),
            "sprite-2",
            &mut residuals,
        );
        assert_eq!(residuals, vec![residual("sprites", "sprite-1")]);
        assert!(deletion_orphans_infrastructure(
            &provider("sprites"),
            Some("sprite-2"),
            &residuals,
        ));
    }

    /// Same id, different provider: coincidental collision between two
    /// providers' id spaces must not retire the other provider's deployment.
    #[test]
    fn an_identical_id_on_another_provider_is_not_the_same_deployment() {
        let mut residuals = vec![residual("blox", "agent-1")];
        reclaim_residual_deployment("sprites", &serde_json::json!({}), "agent-1", &mut residuals);
        assert_eq!(residuals, vec![residual("blox", "agent-1")]);
    }

    // ── deletion_orphans_infrastructure ──────────────────────────────────

    /// The regression Alex found: a migrated agent reads `Local`, so a guard
    /// written against `backend` alone lets the delete through and destroys
    /// the last pointer to a deployment that still holds the key.
    #[test]
    fn a_local_agent_with_a_residual_deployment_still_needs_the_force_flag() {
        assert!(deletion_orphans_infrastructure(
            &BackendKind::Local,
            None,
            &[residual("sprites", "sprite-1")],
        ));
    }

    #[test]
    fn a_deployed_provider_agent_needs_the_force_flag() {
        assert!(deletion_orphans_infrastructure(
            &provider("sprites"),
            Some("sprite-1"),
            &[],
        ));
    }

    #[test]
    fn an_agent_that_never_deployed_anywhere_deletes_freely() {
        assert!(!deletion_orphans_infrastructure(
            &BackendKind::Local,
            None,
            &[]
        ));
        // On a provider but never deployed — no infrastructure exists yet.
        assert!(!deletion_orphans_infrastructure(
            &provider("sprites"),
            None,
            &[]
        ));
    }

    // ── begin_backend_transition ─────────────────────────────────────────

    #[test]
    fn the_fence_admits_one_holder_and_releases_on_drop() {
        let transitions = Mutex::new(None);
        let first = begin_backend_transition_in(&transitions, "npub-a").unwrap();
        assert!(begin_backend_transition_in(&transitions, "npub-a").is_err());
        // A different agent is unaffected — the fence is per-agent, not global.
        let _other = begin_backend_transition_in(&transitions, "npub-b").unwrap();
        drop(first);
        assert!(begin_backend_transition_in(&transitions, "npub-a").is_ok());
    }

    /// The guard must clear its entry even when the fenced operation fails,
    /// or one failed deploy would wedge the agent permanently.
    #[test]
    fn the_fence_releases_after_a_failed_operation() {
        let transitions = Mutex::new(None);
        let result: Result<(), String> = (|| {
            let _fence = begin_backend_transition_in(&transitions, "npub-a")?;
            Err("deploy failed".to_string())
        })();
        assert!(result.is_err());
        assert!(transitions.lock().unwrap().as_ref().unwrap().is_empty());
    }
}
