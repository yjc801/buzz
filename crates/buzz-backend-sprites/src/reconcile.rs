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

use crate::classify::{classify, Action, Observation};
use crate::config::{ProviderConfig, AGENT_HOME};
use crate::env;
use crate::launcher::{self, ProbeReport};
use crate::naming::AgentIdentity;
use crate::provision;
use crate::substrate::{CreateOutcome, SessionMeta, SpriteMeta, Substrate, SubstrateError, UrlAuth};
use std::collections::BTreeMap;
use std::time::Duration;

/// How often the loop re-observes.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The operation deadline (spec §Deploy: 600s). It bounds how long ONE
/// deploy waits synchronously — never when anything is destroyed (nothing
/// ever is).
const DEADLINE: Duration = Duration::from_secs(600);

/// Bounded probe exec. Short: it is a `bash` one-liner over an existing VM,
/// and a slow one is better retried by the loop than waited on.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Run one deploy to a terminal outcome.
pub async fn deploy(
    substrate: &impl Substrate,
    identity: &AgentIdentity,
    cfg: &ProviderConfig,
    env_map: BTreeMap<String, String>,
) -> Result<String, String> {
    let sprite_name = identity.sprite_name();
    let mut attempt_started = false;
    let mut created = false;
    let mut provisioned = false;
    let mut adopted_winner = false;
    let mut last_probe: Option<ProbeReport> = None;
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
            // Resolving the intent needs one exec (`uname -m`), so it only
            // runs when a decision could depend on it.
            let resolved = provision::resolve(substrate, &sprite_name, cfg).await?;
            let recorded = provision::recorded_fingerprint(substrate, &sprite_name).await?;
            (
                probe,
                sessions,
                recorded.map(|f| f.as_str().to_string()),
                resolved.fingerprint().as_str().to_string(),
            )
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
        match classify(&observation) {
            Action::NoOp { agent_id } => return Ok(agent_id),

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
                let resolved = provision::resolve(substrate, &sprite_name, cfg).await?;
                provision::ensure(substrate, &sprite_name, &resolved).await?;
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
