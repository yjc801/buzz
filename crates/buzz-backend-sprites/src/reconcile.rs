//! The deploy loop (spec §Deploy State Machine): converge to at-most-one
//! live agent, keyed on the identity derived from the nsec.
//!
//! Bounds this loop honors:
//! - **One create, one provision, one start per call.** Once this call has
//!   made its own attempt, a classification that would replace it means the
//!   attempt already failed; the answer is an in-band error, never a hot
//!   retry cycle inside the same call.
//! - **Zero destructive substrate calls.** The `Substrate` trait cannot
//!   express a delete, so no row can reach for one.
//! - **Zero mutation on the live row.** A running agent is observed and
//!   returned, never touched — not even to re-assert URL privacy.
//! - **No mutation without the deploy lease.** Provision and Start require
//!   the sprite-wide lease, and the observation that authorizes them is
//!   re-made after acquiring it, so concurrent deploys of one agent
//!   serialize instead of interleaving writes.

use crate::classify::{classify, Action, Observation};
use crate::config::{ProviderConfig, AGENT_HOME};
use crate::env;
use crate::launcher::{self, ProbeReport};
use crate::naming::AgentIdentity;
use crate::provision;
use crate::substrate::{
    CreateOutcome, SessionMeta, SpriteMeta, Substrate, SubstrateError, UrlAuth,
};
use std::collections::BTreeMap;
use std::time::Duration;

/// How often the loop re-observes.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The operation deadline (spec §Deploy: 600s). It bounds how long ONE
/// deploy waits synchronously — never when anything is destroyed (nothing
/// ever is).
///
/// Visible to [`crate::provision`] so an observation step's retry ladder can
/// refuse an attempt this deadline cannot fit, rather than spending the
/// budget on a wait whose answer arrives after the loop has already given up.
pub(crate) const DEADLINE: Duration = Duration::from_secs(600);

/// Bounded probe exec. Short: it is a `bash` one-liner over an existing VM,
/// and a slow one is better retried by the loop than waited on.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// A terminal deploy outcome: the agent's stable handle, plus whether THIS
/// call started the generation now running.
pub struct Deployed {
    pub agent_id: String,
    /// True exactly when this call performed the Start AND the generation
    /// observed running at success is the one that Start booted (the
    /// probe's `gen` names it — env file name, lifecycle correlator, and
    /// probe `gen` are one identity) — so everything that generation's env
    /// carried (the wake replay floor included) is provably in effect.
    /// False for a strict no-op against a generation some earlier deploy
    /// started — including a concurrent rival's (`adopted_winner`), whose
    /// env is not ours — and false when this call's Start was superseded:
    /// the deploy lease expires at 420s while this loop may run 600s, so a
    /// successor can start ITS generation and a stale call must not vouch
    /// for a floor that successor never adopted.
    pub fresh_generation: bool,
}

/// Run one deploy to a terminal outcome.
pub async fn deploy(
    substrate: &impl Substrate,
    identity: &AgentIdentity,
    cfg: &ProviderConfig,
    env_map: BTreeMap<String, String>,
) -> Result<Deployed, String> {
    // The sprite-wide fence: both mutating actions (Provision, Start) run
    // under an in-sprite deploy lease taken with this call's token, so two
    // concurrent deploys against the same *existing* sprite — the case the
    // create-race handling cannot see — never interleave provisioning, and
    // the observation that authorizes a start is always made while the
    // fence is held. The token is provider-minted hex, safe to embed in
    // the lease scripts.
    let lease_token = crate::naming::new_generation();
    let mut lease_held = false;
    let result = deploy_loop(
        substrate,
        identity,
        cfg,
        env_map,
        &lease_token,
        &mut lease_held,
    )
    .await;
    if lease_held {
        // Best-effort: a failure leaves the lease to its TTL, which the
        // fence already tolerates (a crashed deploy never releases either).
        let _ = provision::release_lease(substrate, &identity.sprite_name(), &lease_token).await;
    }
    result
}

async fn deploy_loop(
    substrate: &impl Substrate,
    identity: &AgentIdentity,
    cfg: &ProviderConfig,
    env_map: BTreeMap<String, String>,
    lease_token: &str,
    lease_held: &mut bool,
) -> Result<Deployed, String> {
    let sprite_name = identity.sprite_name();
    let mut attempt_started = false;
    let mut created = false;
    let mut provisioned = false;
    let mut adopted_winner = false;
    let mut last_probe: Option<ProbeReport> = None;
    // The resolved provision intent, computed at most once per deploy call
    // (see [`resolve_once`]). Caching it keeps the desired fingerprint
    // stable for the whole call: a rolling release republished mid-loop
    // cannot flip a just-converged sprite back to "diverged" and trip the
    // one-provision-per-call bound.
    let mut resolved_intent: Option<provision::Resolved> = None;
    // The generation this call actually started, which is NOT the one the
    // caller's env map carries: the loop mints a fresh token per attempt so
    // the env file name, the harness's lifecycle correlator, and the probe's
    // `gen` are one identity. Reporting the caller's token would name a
    // generation that never ran.
    let mut attempt_generation: Option<String> = None;

    loop {
        if substrate.elapsed() >= DEADLINE {
            return Err(startup_not_confirmed(substrate, &sprite_name, last_probe.as_ref()).await);
        }

        // --- Observe -----------------------------------------------------
        let sprite = substrate
            .get_sprite(&sprite_name)
            .await
            .map_err(|SubstrateError(e)| e)?;

        // The auto-repair fence: a sprite under our deterministic name that
        // is not provably ours is never adopted, provisioned, or acted on.
        // Identity evidence and ownership evidence are both required.
        if let Some(meta) = &sprite {
            if !identity.verify_labels(&meta.labels) {
                return Err(format!(
                    "sprite {sprite_name:?} exists but was not created by this \
                     provider for this agent (its labels carry neither our \
                     management marker nor this agent's public key). Nothing \
                     was changed. Inspect it with `sprite info -s \
                     {sprite_name}`, then rename or remove it if it is yours \
                     to remove."
                ));
            }
        }

        let (probe, our_sessions, recorded_intent, desired_intent) = if sprite.is_some() {
            let probe = probe_once(substrate, &sprite_name).await?;
            if probe.is_some() {
                last_probe = probe.clone();
            }
            let sessions = our_sessions(substrate, &sprite_name, identity).await?;
            let mut recorded = provision::recorded_fingerprint(substrate, &sprite_name)
                .await?
                .map(|f| f.as_str().to_string());
            // The desired fingerprint costs one exec (`uname -m`) plus —
            // whenever the owner pinned no digest — an external fetch of
            // the release's published one. So it is resolved lazily, under
            // exactly the conditions where `classify` could reach its
            // recorded-vs-desired comparison: race not lost, an intent on
            // record, a definitely-stopped probe (which also rules out a
            // live harness), no lingering session, no spent attempt. Every
            // earlier classify row answers without it, which keeps a
            // healthy no-op deploy — and every polling iteration —
            // independent of GitHub release availability. This gate must
            // stay in lockstep with `classify`'s row order: a condition
            // missed here would feed the comparison an empty desired
            // fingerprint and provision spuriously.
            let could_compare_intent = !adopted_winner
                && recorded.is_some()
                && probe.as_ref().is_some_and(ProbeReport::stopped)
                && sessions.is_empty()
                && !attempt_started;
            let desired = if could_compare_intent {
                resolve_once(&mut resolved_intent, substrate, &sprite_name, cfg)
                    .await?
                    .fingerprint()
                    .as_str()
                    .to_string()
            } else {
                String::new()
            };
            // A matching fingerprint is only a fast path (the intent doc's
            // rule: evidence over recollection). Before it can lead to a
            // Start, the installed sprig must still hash to what
            // install-time recorded; a failure — deleted, truncated, or
            // replaced binary — reads as divergence, and the full-provision
            // path repairs it. Gated on exactly the conditions under which
            // `classify` could answer Start, so ordinary polling iterations
            // never pay the extra exec.
            if could_compare_intent
                && recorded.as_deref() == Some(desired.as_str())
                && !provision::spot_check(substrate, &sprite_name).await?
            {
                recorded = None;
            }
            (probe, sessions, recorded, desired)
        } else {
            (None, Vec::new(), None, String::new())
        };

        let observation = Observation {
            sprite: sprite.as_ref(),
            probe: probe.as_ref(),
            our_sessions: &our_sessions,
            recorded_intent: recorded_intent.as_deref(),
            desired_intent: &desired_intent,
            started_this_call: attempt_started,
            adopted_winner,
        };

        // --- Act ---------------------------------------------------------
        let action = classify(&observation);

        // The fence gate: no mutating action without the deploy lease. On
        // acquisition the loop deliberately re-observes instead of acting —
        // the observation that authorizes a provision or start must itself
        // be made under the fence, or a rival's writes could land between
        // the read and the act. NoOp/Observe paths never take the lease, so
        // a live agent's sprite is never written to.
        if matches!(action, Action::Provision | Action::Start) && !*lease_held {
            match provision::acquire_lease(substrate, &sprite_name, lease_token).await? {
                provision::LeaseAttempt::Acquired => *lease_held = true,
                provision::LeaseAttempt::HeldByAnother => substrate.sleep(POLL_INTERVAL).await,
            }
            continue;
        }

        match action {
            // Every success returns through NoOp, but `attempt_started`
            // alone is NOT the fresh-generation answer: it stays true after
            // this call's Start even if the observed running harness is a
            // successor's (the deploy lease expires at 420s, this loop may
            // run 600s). The claim is therefore fenced by identity: only a
            // running probe whose `gen` equals the generation this call
            // started proves the floor-bearing env is in effect. Fail
            // closed — no probe, no started generation, or a mismatch all
            // report false (unproven, never a guess).
            Action::NoOp { agent_id } => {
                let fresh_generation = attempt_started
                    && match (attempt_generation.as_deref(), probe.as_ref()) {
                        (Some(started), Some(running)) => running.gen == started,
                        _ => false,
                    };
                return Ok(Deployed {
                    agent_id,
                    fresh_generation,
                });
            }

            Action::Observe { .. } => substrate.sleep(POLL_INTERVAL).await,

            Action::ReportStartupFailure => {
                return Err(harness_exited_during_startup(
                    attempt_generation.as_deref(),
                    last_probe.as_ref(),
                ))
            }

            Action::Create => {
                if created {
                    // Our own create raced with something that removed the
                    // sprite; one create per call, so report rather than spin.
                    return Err(format!(
                        "sprite {sprite_name:?} disappeared after this deploy created it — \
                         nothing was retried. Start the agent again."
                    ));
                }
                created = true;
                match substrate
                    .create_sprite(&sprite_name, &identity.labels())
                    .await
                    .map_err(|SubstrateError(e)| e)?
                {
                    CreateOutcome::Created(_) => {}
                    // We lost: a concurrent deploy of this agent owns the
                    // sprite. It is still verified by the next iteration's
                    // label check, but from here we only observe it.
                    CreateOutcome::AlreadyExists => adopted_winner = true,
                    CreateOutcome::CreationRateLimited { retry_after } => {
                        // Not a spent attempt: nothing was created.
                        created = false;
                        let remaining = DEADLINE.saturating_sub(substrate.elapsed());
                        if retry_after >= remaining {
                            return Err(format!(
                                "Fly Sprites is rate-limiting sprite creation and asks for \
                                 {}s, which exceeds this deploy's remaining budget. Start \
                                 the agent again shortly.",
                                retry_after.as_secs()
                            ));
                        }
                        substrate.sleep(retry_after).await;
                    }
                    CreateOutcome::ConcurrentLimit { message } => {
                        return Err(format!(
                            "Fly Sprites refused to create {sprite_name:?}: {message}. \
                             Stop or delete an idle sprite, or raise the organization's \
                             concurrent-sprite limit, then start the agent again."
                        ))
                    }
                }
            }

            Action::Provision => {
                if provisioned {
                    return Err(format!(
                        "sprite {sprite_name:?} still reports diverged provisioning after \
                         this deploy converged it — nothing was retried. Start the agent \
                         again."
                    ));
                }
                provisioned = true;
                let resolved =
                    resolve_once(&mut resolved_intent, substrate, &sprite_name, cfg).await?;
                provision::ensure(substrate, &sprite_name, resolved, lease_token).await?;
                // URL privacy is asserted at create and re-asserted here —
                // provision is the only place this binding writes to a
                // sprite it did not just create, and never on the live row.
                if sprite
                    .as_ref()
                    .and_then(|s| s.url_auth.as_deref())
                    .is_some_and(|auth| auth != UrlAuth::Sprite.as_str())
                {
                    substrate
                        .set_url_settings(&sprite_name, UrlAuth::Sprite)
                        .await
                        .map_err(|SubstrateError(e)| e)?;
                }
            }

            Action::Start => {
                // `lease_held` is in-memory truth; the durable lease expires
                // after its TTL. If anything above stalled past it, a
                // successor may have acquired the fence and provisioned —
                // so re-confirm (and refresh) ownership immediately before
                // the one mutating step `ensure`'s per-step guards cannot
                // cover. A failed confirmation means the observation that
                // chose Start predates a successor's writes: discard it and
                // re-enter the loop, which re-acquires and re-observes.
                if !provision::confirm_lease(substrate, &sprite_name, lease_token).await? {
                    *lease_held = false;
                    substrate.sleep(POLL_INTERVAL).await;
                    continue;
                }
                attempt_started = true;
                // Fresh generation per attempt: the env file's name, the
                // lifecycle correlator, and the probe's `gen` are one
                // identity.
                let generation = crate::naming::new_generation();
                attempt_generation = Some(generation.clone());
                let mut attempt_env = env_map.clone();
                attempt_env.insert(env::START_NONCE_KEY.to_string(), generation.clone());

                write_env_file(substrate, &sprite_name, &generation, &attempt_env).await?;
                substrate
                    .start_detached(
                        &sprite_name,
                        &launcher::launcher_argv(identity.pubkey_hex(), &generation),
                        AGENT_HOME,
                    )
                    .await
                    .map_err(|SubstrateError(e)| {
                        format!("could not start the agent session: {e}")
                    })?;
                substrate.sleep(POLL_INTERVAL).await;
            }
        }
    }
}

/// Resolve the provision intent at most once per deploy call.
///
/// Resolution is the loop's only step with a dependency outside the sprite:
/// without an owner-pinned digest it fetches the one the release publishes.
/// Memoizing it means a release outage cannot fail a deploy that never
/// needed to provision, repeated polling performs no redundant downloads,
/// and the desired fingerprint this call provisions is the one it then
/// compares against.
async fn resolve_once<'a>(
    cache: &'a mut Option<provision::Resolved>,
    substrate: &impl Substrate,
    sprite: &str,
    cfg: &ProviderConfig,
) -> Result<&'a provision::Resolved, String> {
    match cache {
        Some(resolved) => Ok(resolved),
        empty => Ok(empty.insert(provision::resolve(substrate, sprite, cfg).await?)),
    }
}

/// Stream the resolved environment into a per-attempt tmpfs file.
///
/// `/dev/shm` is RAM-backed, so the nsec never reaches the durable,
/// object-storage-synced filesystem and cannot enter a checkpoint. Written
/// under `umask 077` to a temp name and renamed, so no torn file is ever
/// observable, and removed by the launcher before it execs the harness.
async fn write_env_file(
    substrate: &impl Substrate,
    sprite: &str,
    generation: &str,
    env_map: &BTreeMap<String, String>,
) -> Result<(), String> {
    let path = format!("/dev/shm/buzz-agent.{generation}.env");
    let script = format!("umask 077; cat > {path}.tmp && mv {path}.tmp {path}");
    let result = substrate
        .run(
            sprite,
            &["bash".to_string(), "-c".to_string(), script],
            Some(env::serialize_exports(env_map).into_bytes()),
            Duration::from_secs(60),
        )
        .await
        .map_err(|SubstrateError(e)| format!("could not stage the agent environment: {e}"))?;
    if result.exit_code != 0 {
        return Err(format!(
            "could not stage the agent environment (exit {})",
            result.exit_code
        ));
    }
    Ok(())
}

async fn probe_once(
    substrate: &impl Substrate,
    sprite: &str,
) -> Result<Option<ProbeReport>, String> {
    match substrate
        .run(sprite, &launcher::probe_argv(), None, PROBE_TIMEOUT)
        .await
    {
        Ok(result) => Ok(ProbeReport::parse(&result.stdout)),
        // A probe that cannot run is missing evidence, not evidence of
        // absence: the loop polls again rather than treating it as stopped.
        Err(SubstrateError(_)) => Ok(None),
    }
}

/// Sessions whose argv names our launcher for this identity. Corroboration
/// only — `is_active` reports client attachment, not process liveness.
async fn our_sessions(
    substrate: &impl Substrate,
    sprite: &str,
    identity: &AgentIdentity,
) -> Result<Vec<SessionMeta>, String> {
    let sessions = match substrate.list_sessions(sprite).await {
        Ok(sessions) => sessions,
        // Same reasoning as the probe: absence of evidence is not evidence.
        Err(SubstrateError(_)) => return Ok(Vec::new()),
    };
    Ok(sessions
        .into_iter()
        .filter(|s| {
            s.command.contains(launcher::LAUNCHER_PATH) && s.command.contains(identity.pubkey_hex())
        })
        .collect())
}

/// The deadline report: machine-readable tokens only (probe fields and the
/// sprite's own status), never process-composed output.
async fn startup_not_confirmed(
    substrate: &impl Substrate,
    sprite_name: &str,
    probe: Option<&ProbeReport>,
) -> String {
    let status = substrate
        .get_sprite(sprite_name)
        .await
        .ok()
        .flatten()
        .map(|s: SpriteMeta| s.status)
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "startup not confirmed within the deadline: sprite {sprite_name:?} is {status}, \
         {}. Nothing was removed; starting the agent again picks up wherever this \
         attempt got to.",
        describe(probe)
    )
}

fn harness_exited_during_startup(
    attempt_generation: Option<&str>,
    probe: Option<&ProbeReport>,
) -> String {
    let generation = attempt_generation.unwrap_or("unknown");
    format!(
        "the agent exited during startup (generation {generation}): {}. Nothing was \
         retried automatically — start the agent again once its configuration is \
         corrected.",
        describe(probe)
    )
}

fn describe(probe: Option<&ProbeReport>) -> String {
    match probe {
        Some(p) => format!(
            "the last probe reported lock={}, process={:?}, generation={:?}",
            if p.lock_held { "held" } else { "free" },
            p.comm,
            p.gen
        ),
        None => "no probe report was produced".to_string(),
    }
}

#[cfg(test)]
mod tests;
