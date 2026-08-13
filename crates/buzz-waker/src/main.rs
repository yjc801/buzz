//! `buzz-waker` daemon entry point.
//!
//! Env-var configured — this repo's other daemons don't use clap; see
//! `crates/buzz-relay/src/main.rs` and `crates/buzz-pair-relay/src/main.rs`
//! for the pattern this follows. JSON-structured logs, graceful shutdown on
//! SIGTERM/Ctrl+C via a shared [`CancellationToken`], and three tasks spawned
//! per watched agent: the mention-feed loop
//! ([`buzz_waker::wake_loop::run_wake_loop`]), the presence tap
//! ([`buzz_waker::presence_feed::run_presence_tap`]), and the bundle-delivery
//! tap ([`buzz_waker::bundle_feed::run_bundle_tap`]).
//!
//! # Two ways an agent gets watched
//!
//! **Statically**, from `WAKER_AGENTS_CONFIG_PATH` — read once at startup,
//! same as before this module's dynamic supervisor (below) existed.
//!
//! **Dynamically**, via the roster tap
//! ([`buzz_waker::roster_feed::run_roster_tap`]), when `WAKER_OWNER_PUBKEYS`
//! is non-empty (`PLANS/BUZZ_WAKER_DESIGN.md` §12 build order step 3). This
//! daemon's own reconciliation loop diffs every authorized owner's current
//! roster against its own `supervised` map, spawns a per-agent credential tap
//! ([`buzz_waker::credential_feed::run_credential_tap`]) to fetch a
//! newly-listed agent's `nsec` before it can be watched at all, then calls
//! the same [`spawn_agent_watch`] a static agent uses once that credential
//! arrives — and cancels a previously roster-added agent's tasks the moment
//! it drops off every authorized owner's roster. A statically configured
//! agent always wins a pubkey collision against a roster-discovered one: see
//! [`compute_desired_roster_agents`]'s own doc.
//!
//! # Configuration
//!
//! | Env var | Required | Meaning |
//! |---|---|---|
//! | `WAKER_RELAY_URL` | yes | The relay every watched agent's mention feed, presence tap, and bundle tap connects to — also the relay the roster/credential taps below connect to. |
//! | `WAKER_STATE_DIR` | yes | Base directory for durable per-agent state (`<dir>/<pubkey>/{cursor,floor,credential_floor}.json`) and the roster's own per-owner floors (`<dir>/roster-floors/<owner>.json`). Created if missing. |
//! | `WAKER_AGENTS_CONFIG_PATH` | yes | Path to a JSON file listing the agents to statically watch — see [`AgentConfig`]. |
//! | `WAKER_OWNER_PUBKEYS` | no | Comma-separated list of owner pubkeys this daemon discovers agents for dynamically. Empty or unset disables dynamic enrolment entirely — see [`buzz_waker::enrolment::parse_authorized_owners`]'s own fail-closed doc. |
//! | `WAKER_IDENTITY_NSEC` | only if `WAKER_OWNER_PUBKEYS` is set | This daemon's own Nostr identity — the roster and credential taps decrypt as this key, never as any watched agent's. |
//! | `RUST_LOG` | no | `tracing-subscriber` env filter. Defaults to `buzz_waker=info`. |
//!
//! # What is still deliberately not here
//!
//! Every *statically* configured agent's identity and owner pubkey (pinned
//! into its [`buzz_waker::floors::FloorStore`] on first run, **G2**) are read
//! from local config, matching the ecosystem's existing agent-identity
//! provisioning story — nothing about *that* pin can come from a delivered
//! bundle without defeating the pin's own purpose. A dynamically discovered
//! agent's owner is instead the roster entry's own owner, already proven
//! against `WAKER_OWNER_PUBKEYS` before this daemon ever trusts it (see
//! [`buzz_waker::roster_feed`]'s module doc).
//!
//! Not implemented this round, and deliberately deferred rather than
//! guessed at: a `WAKER_MAX_AGENTS` total-capacity bound (recorded as an
//! open tuning value in the design doc's multi-tenant extension, not part
//! of this step's own build-order text) and reacting to a credential
//! *rotation* for an already-running dynamically watched agent (this
//! daemon's credential tap keeps running for that agent's whole lifetime
//! and would log a rotation or revocation, but nothing currently acts on it
//! — only the *first* delivered credential is used, to bootstrap that
//! agent's identity).

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nostr::{Keys, Tag};
use serde::Deserialize;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use buzz_waker::bundle_feed::{run_bundle_tap, BundleState};
use buzz_waker::credential_feed::{run_credential_tap, CredentialState};
use buzz_waker::decide::normalize_pubkey;
use buzz_waker::enrolment::{parse_authorized_owners, RosterEntry};
use buzz_waker::floors::{FloorError, FloorStore};
use buzz_waker::presence_feed::{run_presence_tap, PresenceState};
use buzz_waker::roster_feed::{run_roster_tap, RosterState};
use buzz_waker::wake_loop::{run_wake_loop, WakeLoopConfig};
use buzz_waker::watch_list::WatchList;

/// One watched agent, as read from `WAKER_AGENTS_CONFIG_PATH`.
#[derive(Debug, Deserialize)]
struct AgentConfig {
    /// The agent's own Nostr private key, hex or `nsec1...` — [`Keys::parse`]
    /// accepts either. This identity is what the mention feed and the
    /// presence tap both authenticate as, per §4 option (ii) of the design:
    /// one connection per watched agent, bound to that agent's own identity.
    nsec: String,
    /// Raw NIP-OA authorization tag, e.g. `["auth", "<token>"]`, if this
    /// relay deployment requires one to accept the connection. Most
    /// deployments do not — the interactive per-agent NIP-42 auth already
    /// identifies the connection correctly, and the deployment amendment in
    /// §4 of the design records that this fork's relay does not implement
    /// the restricted connection class this tag would otherwise request.
    #[serde(default)]
    auth_tag: Option<Vec<String>>,
    /// The owner's pubkey, hex — pinned into this agent's [`FloorStore`] on
    /// first run (**G2**) and used to scope the bundle-delivery query's
    /// `authors` filter. Read from local config, not from any delivered
    /// bundle: the pin has to exist independently of anything a bundle could
    /// claim about itself, or a compromised bundle could pin its own
    /// attacker-controlled owner.
    owner_pubkey: String,
}

/// Normalize and validate a configured `owner_pubkey` as a real Nostr public
/// key, before any per-agent state directory or [`FloorStore`] is created.
///
/// `owner_pubkey` is otherwise only normalized (trimmed/lowercased) and
/// never parsed until a bundle actually arrives and `bundle_feed` calls
/// [`nostr::PublicKey::from_hex`] on it — by which point a typo has already
/// been durably enrolled as the floor's pinned owner (G2, which never
/// rewrites once set). That leaves the daemon reporting healthy startup
/// while silently unable to admit anything, and correcting the config on a
/// later run is then refused by [`ensure_owner_pin_matches`]. Validating
/// here, before enrollment, catches the typo at startup instead.
///
/// # Errors
/// `owner_pubkey` does not parse as a hex Nostr public key.
fn parse_owner_pubkey(pubkey: &str, owner_pubkey: &str) -> anyhow::Result<String> {
    let owner_pubkey = normalize_pubkey(owner_pubkey);
    nostr::PublicKey::from_hex(&owner_pubkey).map_err(|e| {
        anyhow::anyhow!("invalid owner_pubkey for agent {pubkey} in WAKER_AGENTS_CONFIG_PATH: {e}")
    })?;
    Ok(owner_pubkey)
}

/// Refuse to start `pubkey`'s watch tasks if the floor's pinned owner
/// disagrees with what `WAKER_AGENTS_CONFIG_PATH` configures now.
///
/// The floor's owner is pinned once, at enrolment, and never rewritten (G2)
/// — but config is free to change on any later run. If it drifts from the
/// pin, the daemon would still start "successfully" while
/// subscribing/decrypting as the newly configured owner and verifying every
/// delivery against the old pinned one, so no bundle could ever be admitted.
/// Both arguments are expected already-normalized ([`normalize_pubkey`]).
///
/// # Errors
/// The two owners disagree.
fn ensure_owner_pin_matches(
    pubkey: &str,
    pinned_owner: &str,
    configured_owner: &str,
) -> anyhow::Result<()> {
    if pinned_owner != configured_owner {
        anyhow::bail!(
            "floor store for {pubkey} is pinned to owner {pinned_owner}, but \
             the configured owner is now {configured_owner}; refusing to run with \
             disagreeing owners"
        );
    }
    Ok(())
}

fn env_var(name: &str) -> anyhow::Result<String> {
    std::env::var(name).map_err(|_| anyhow::anyhow!("{name} is required but not set"))
}

fn log_env_filter() -> EnvFilter {
    EnvFilter::new(std::env::var("RUST_LOG").unwrap_or_else(|_| "buzz_waker=info".to_string()))
}

/// Load and validate the watch list from `path`.
///
/// # Errors
/// The file is missing or unreadable, its JSON does not match
/// [`AgentConfig`]'s shape, or it lists zero agents — a daemon with nothing
/// to watch is a misconfiguration, not a valid idle state.
fn load_agents(path: &str) -> anyhow::Result<Vec<AgentConfig>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("could not read WAKER_AGENTS_CONFIG_PATH {path}: {e}"))?;
    let agents: Vec<AgentConfig> = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("could not parse WAKER_AGENTS_CONFIG_PATH {path}: {e}"))?;
    if agents.is_empty() {
        anyhow::bail!("WAKER_AGENTS_CONFIG_PATH {path} lists no agents; nothing to watch");
    }
    Ok(agents)
}

/// Wait for SIGTERM (Unix) or Ctrl+C — matches
/// `crates/buzz-relay/src/main.rs`'s `shutdown_signal`.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(sigterm) => sigterm,
            Err(error) => {
                tracing::error!(
                    %error,
                    "buzz-waker: could not install a SIGTERM handler; Ctrl+C only"
                );
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// How a watched pubkey came to be watched — governs both dedup ordering
/// (config always wins a collision) and how loudly this daemon reacts to
/// one of its tasks exiting unsolicited: see [`classify_exit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentSource {
    /// Listed in `WAKER_AGENTS_CONFIG_PATH`.
    Config,
    /// Discovered via an authorized owner's roster.
    Roster,
}

/// This daemon's bookkeeping for one currently-watched (or being-bootstrapped)
/// pubkey.
struct SupervisedAgent {
    /// Shared by every task this pubkey owns (credential tap, presence tap,
    /// bundle tap, wake loop) — a [`CancellationToken::child_token`] of the
    /// daemon's own global token. Cancelling it stops exactly this pubkey's
    /// tasks, nothing else; the global token cancelling stops it too, along
    /// with everything else, on shutdown.
    cancel: CancellationToken,
    source: AgentSource,
    /// `Some` while a [`AgentSource::Roster`] agent is still waiting for its
    /// first credential delivery — the reconciliation loop polls it each
    /// tick and, once populated, spawns this agent's presence/bundle/wake
    /// tasks and clears this field. Always `None` for [`AgentSource::Config`]
    /// (which already has its `nsec` from local config) and for a
    /// [`AgentSource::Roster`] agent whose bootstrap has already completed.
    credential_bootstrap: Option<Arc<CredentialState>>,
    /// The owner that published this pubkey's roster entry. Unused for
    /// [`AgentSource::Config`] (each config entry already carries its own
    /// `owner_pubkey` separately, read once at spawn time).
    owner_pubkey: String,
}

/// Why one of this daemon's tasks finished, for the join-handling loop in
/// [`main`] to classify via [`classify_exit`].
enum TaskExit {
    /// One of a watched agent's three tasks (`"presence_tap"`,
    /// `"bundle_tap"`, or `"wake_loop"`).
    Agent { pubkey: String, task: &'static str },
    /// A [`AgentSource::Roster`] agent's credential tap, tracked separately
    /// from `Agent` only so log lines name it correctly — it shares that
    /// agent's own [`SupervisedAgent::cancel`] and is classified exactly the
    /// same way.
    CredentialTap { pubkey: String },
    /// A daemon-wide task with no per-agent scope: the roster tap. Expected
    /// to run until the global token cancels; any other exit is fatal.
    Component(&'static str),
}

/// What an unsolicited task exit means for the daemon.
#[derive(Debug, PartialEq, Eq)]
enum ExitDisposition {
    /// Already accounted for — the owning entry was already removed from
    /// `supervised` (an earlier sibling's exit already tore this pubkey
    /// down), or its own token was already cancelled deliberately. No
    /// further action.
    Expected,
    /// This daemon cannot recover from this exit; stop everything.
    FatalDaemon,
    /// Tear down just this one roster-discovered agent's remaining tasks
    /// and keep running everything else — a single tenant's agent
    /// misbehaving must not take down a daemon serving several.
    TearDownAgent,
}

/// Classify one task's exit, given whether it was already-known-cancelled at
/// the moment it finished and which agent (if any) it belonged to.
///
/// `was_cancelled = true` covers both a deliberate per-agent cancellation
/// (roster removed this pubkey) and a global shutdown (every child token
/// cancels transitively) — either way, the exit is expected, not a failure
/// this function needs to react to.
///
/// A statically configured agent's unsolicited exit is fatal for the whole
/// daemon — the historical behavior, preserved exactly: before this
/// module's dynamic supervisor, *every* watched agent was config-sourced,
/// so this is also what makes today's default (no `WAKER_OWNER_PUBKEYS`)
/// behave identically to before this file changed.
///
/// `source = None` only reaches this function as `(None, true)` from the
/// real call site in [`main`] — it derives `was_cancelled` via
/// `supervised.get(pubkey).map(|a| a.cancel.is_cancelled()).unwrap_or(true)`,
/// so a pubkey no longer in `supervised` (already torn down by an earlier
/// sibling task's exit) always reports `was_cancelled = true` and short
/// circuits above. `(None, false)` is therefore not reachable from `main`
/// today; it is still handled here, conservatively, as fatal rather than
/// `unreachable!` — a caller that ever changes that default should fail
/// loud, not silently swallow an exit this function has no real source for.
fn classify_exit(source: Option<AgentSource>, was_cancelled: bool) -> ExitDisposition {
    if was_cancelled {
        return ExitDisposition::Expected;
    }
    match source {
        None | Some(AgentSource::Config) => ExitDisposition::FatalDaemon,
        Some(AgentSource::Roster) => ExitDisposition::TearDownAgent,
    }
}

/// One agent this daemon should be watching because an authorized owner's
/// roster lists it.
struct DesiredRosterAgent {
    pubkey: String,
    owner_pubkey: String,
}

/// Diff every authorized owner's current roster into the set of pubkeys this
/// daemon should be watching dynamically.
///
/// `rosters` is `(owner_pubkey, entries)` for every owner this daemon
/// currently has a tracked roster for (from
/// [`buzz_waker::roster_feed::RosterState`] — read at the call site, not
/// here, so this stays a plain function over data rather than needing a live
/// `RosterState` to unit test). `existing_sources` is a snapshot of
/// `main`'s own `supervised` map, pubkey to [`AgentSource`] — enough to
/// enforce the one dedup rule that matters: **a statically configured
/// pubkey is never touched by this diff**, in either direction. It is never
/// added again (it is already supervised) and never removed (removal below
/// only ever targets [`AgentSource::Roster`] entries) — see
/// `PLANS/BUZZ_WAKER_DESIGN.md` §12's own note that a broken enrolment path
/// must never be load-bearing for an agent an operator explicitly
/// configured.
///
/// Returns the desired set plus every pubkey more than one authorized owner
/// claims in this pass — informational for the caller to log loudly (an
/// operator misconfiguration, not something this function can resolve on
/// its own); the first owner encountered wins that pubkey, deterministic
/// only in `rosters`' own iteration order.
fn compute_desired_roster_agents(
    rosters: &[(String, Vec<RosterEntry>)],
    existing_sources: &HashMap<String, AgentSource>,
) -> (Vec<DesiredRosterAgent>, Vec<String>) {
    let mut desired = Vec::new();
    let mut claimed_by: HashMap<String, String> = HashMap::new();
    let mut conflicts = Vec::new();

    for (owner_pubkey, entries) in rosters {
        for entry in entries {
            let pubkey = normalize_pubkey(&entry.agent_pubkey);
            if existing_sources.get(&pubkey) == Some(&AgentSource::Config) {
                continue;
            }
            match claimed_by.get(&pubkey) {
                Some(first_owner) if first_owner != owner_pubkey => {
                    conflicts.push(pubkey);
                }
                Some(_) => {}
                None => {
                    claimed_by.insert(pubkey.clone(), owner_pubkey.clone());
                    desired.push(DesiredRosterAgent {
                        pubkey,
                        owner_pubkey: owner_pubkey.clone(),
                    });
                }
            }
        }
    }

    (desired, conflicts)
}

/// Open (or, the first time this daemon has ever seen `pubkey` under this
/// exact floor path, enroll) a [`FloorStore`] pinned to `owner_pubkey`, and
/// refuse it if a previous pin disagrees.
///
/// Shared by the bundle floor and the credential floor, and by both a
/// statically configured agent (whose owner never changes across restarts)
/// and a roster-discovered one (whose claimed owner is re-validated against
/// the pin on every reconciliation tick that touches it).
///
/// # Errors
/// The store cannot be created/opened, or its pinned owner disagrees with
/// `owner_pubkey`.
fn open_pinned_floor_store(path: &Path, owner_pubkey: &str) -> anyhow::Result<FloorStore> {
    let store = match FloorStore::open(path) {
        Ok(store) => store,
        Err(FloorError::NotEnrolled { .. }) => {
            FloorStore::enroll(path, owner_pubkey).map_err(|e| {
                anyhow::anyhow!("could not enroll floor store at {}: {e}", path.display())
            })?
        }
        Err(e) => anyhow::bail!("could not open floor store at {}: {e}", path.display()),
    };
    let pinned_owner = normalize_pubkey(&store.snapshot().owner_pubkey);
    ensure_owner_pin_matches(&path.display().to_string(), &pinned_owner, owner_pubkey)?;
    Ok(store)
}

/// Spawn one agent's presence tap, bundle tap, and wake loop under `cancel`,
/// and register it in `watch_list`.
///
/// The extracted "per-agent spawn block"
/// `PLANS/BUZZ_WAKER_DESIGN.md` §12 build order step 3 calls for — shared by
/// both a statically configured agent (called once per entry at startup)
/// and a roster-discovered one (called once its credential tap delivers a
/// first `nsec`), so there is exactly one place this wiring can drift.
///
/// # Errors
/// The agent's state directory or bundle [`FloorStore`] cannot be
/// created/opened, or the store's pinned owner disagrees with
/// `owner_pubkey` — see [`open_pinned_floor_store`].
#[allow(clippy::too_many_arguments)]
fn spawn_agent_watch(
    relay_url: &str,
    state_dir: &Path,
    keys: &Keys,
    auth_tag: Option<&Tag>,
    owner_pubkey: &str,
    watch_list: &WatchList,
    cancel: CancellationToken,
    tasks: &mut JoinSet<TaskExit>,
) -> anyhow::Result<()> {
    let pubkey = normalize_pubkey(&keys.public_key().to_hex());
    let agent_dir = state_dir.join(&pubkey);
    std::fs::create_dir_all(&agent_dir)
        .map_err(|e| anyhow::anyhow!("could not create state dir {}: {e}", agent_dir.display()))?;
    let cursor_path = agent_dir.join("cursor.json");
    let floor_path = agent_dir.join("floor.json");

    let mut floor_store = open_pinned_floor_store(&floor_path, owner_pubkey)?;

    let presence_state = Arc::new(PresenceState::new());
    let bundle_state = Arc::new(BundleState::new());

    tracing::info!(agent = %pubkey, owner = %owner_pubkey, "buzz-waker: watching agent");

    {
        let relay_url = relay_url.to_string();
        let keys = keys.clone();
        let auth_tag = auth_tag.cloned();
        let presence_state = Arc::clone(&presence_state);
        let cancel = cancel.clone();
        let pubkey = pubkey.clone();
        tasks.spawn(async move {
            run_presence_tap(
                &relay_url,
                &keys,
                auth_tag.as_ref(),
                &presence_state,
                &cancel,
            )
            .await;
            TaskExit::Agent {
                pubkey,
                task: "presence_tap",
            }
        });
    }

    {
        let relay_url = relay_url.to_string();
        let keys = keys.clone();
        let auth_tag = auth_tag.cloned();
        let owner_pubkey = owner_pubkey.to_string();
        let bundle_state = Arc::clone(&bundle_state);
        let cancel = cancel.clone();
        let pubkey = pubkey.clone();
        tasks.spawn(async move {
            run_bundle_tap(
                &relay_url,
                &keys,
                auth_tag.as_ref(),
                &owner_pubkey,
                &mut floor_store,
                &bundle_state,
                &cancel,
            )
            .await;
            TaskExit::Agent {
                pubkey,
                task: "bundle_tap",
            }
        });
    }

    {
        let config = WakeLoopConfig {
            relay_url: relay_url.to_string(),
            keys: keys.clone(),
            auth_tag: auth_tag.cloned(),
            cursor_path,
            presence_state,
            watch_list: watch_list.clone(),
            bundle_state,
        };
        let cancel = cancel.clone();
        let pubkey = pubkey.clone();
        tasks.spawn(async move {
            run_wake_loop(config, cancel).await;
            TaskExit::Agent {
                pubkey,
                task: "wake_loop",
            }
        });
    }

    watch_list.insert(&pubkey);
    Ok(())
}

/// How often the reconciliation loop re-diffs every authorized owner's
/// current [`RosterState`] against `supervised`.
///
/// This only reads state this daemon already holds in memory (the roster
/// tap keeps `RosterState` current via its own live subscription, not
/// polling) and checks each pending agent's [`CredentialState`] — both
/// cheap — so a short interval costs nothing but stays far from a busy
/// loop.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// One reconciliation pass: tear down roster-sourced agents no longer
/// listed anywhere, adopt newly-listed ones (spawning a credential tap for
/// each), and promote any pending agent whose credential has now arrived.
#[allow(clippy::too_many_arguments)]
fn reconcile_roster(
    relay_url: &str,
    state_dir: &Path,
    waker_keys: &Keys,
    authorized_owners: &[String],
    roster_state: &RosterState,
    supervised: &mut HashMap<String, SupervisedAgent>,
    watch_list: &WatchList,
    cancel: &CancellationToken,
    tasks: &mut JoinSet<TaskExit>,
) {
    let rosters: Vec<(String, Vec<RosterEntry>)> = authorized_owners
        .iter()
        .filter_map(|owner| {
            roster_state
                .current(owner)
                .map(|body| (owner.clone(), body.entries.clone()))
        })
        .collect();

    let existing_sources: HashMap<String, AgentSource> = supervised
        .iter()
        .map(|(pubkey, agent)| (pubkey.clone(), agent.source))
        .collect();
    let (desired, conflicts) = compute_desired_roster_agents(&rosters, &existing_sources);
    for pubkey in conflicts {
        tracing::warn!(
            agent = %pubkey,
            "buzz-waker: more than one authorized owner's roster claims this agent; \
             the first one seen this pass wins, this is almost certainly a \
             misconfiguration"
        );
    }

    let desired_pubkeys: HashSet<&str> = desired.iter().map(|d| d.pubkey.as_str()).collect();
    let to_remove: Vec<String> = supervised
        .iter()
        .filter(|(pubkey, agent)| {
            agent.source == AgentSource::Roster && !desired_pubkeys.contains(pubkey.as_str())
        })
        .map(|(pubkey, _)| pubkey.clone())
        .collect();
    for pubkey in to_remove {
        if let Some(agent) = supervised.remove(&pubkey) {
            agent.cancel.cancel();
        }
        watch_list.remove(&pubkey);
        tracing::info!(
            agent = %pubkey,
            "buzz-waker: no authorized owner's roster lists this agent anymore; \
             cancelling its watch tasks"
        );
    }

    for desired_agent in &desired {
        if supervised.contains_key(&desired_agent.pubkey) {
            continue;
        }
        let agent_cancel = cancel.child_token();
        let agent_dir = state_dir.join(&desired_agent.pubkey);
        if let Err(error) = std::fs::create_dir_all(&agent_dir) {
            tracing::error!(
                agent = %desired_agent.pubkey,
                %error,
                "buzz-waker: could not create state dir for a roster-discovered agent; skipping this pass"
            );
            continue;
        }
        let credential_floor_path = agent_dir.join("credential_floor.json");
        let mut credential_floor_store = match open_pinned_floor_store(
            &credential_floor_path,
            &desired_agent.owner_pubkey,
        ) {
            Ok(store) => store,
            Err(error) => {
                tracing::error!(
                    agent = %desired_agent.pubkey,
                    %error,
                    "buzz-waker: could not open this agent's credential floor; skipping this pass"
                );
                continue;
            }
        };

        // Known the moment this daemon adopts the pubkey, ahead of the
        // credential that proves this daemon can actually run it — see
        // `crate::watch_list`'s own doc for why that ordering is the safe
        // one for `confirm_author_not_known_agent`.
        watch_list.insert(&desired_agent.pubkey);

        let credential_state = Arc::new(CredentialState::new());
        {
            let relay_url = relay_url.to_string();
            let waker_keys = waker_keys.clone();
            let owner_pubkey = desired_agent.owner_pubkey.clone();
            let agent_pubkey = desired_agent.pubkey.clone();
            let credential_state = Arc::clone(&credential_state);
            let tap_cancel = agent_cancel.clone();
            let pubkey_for_exit = desired_agent.pubkey.clone();
            tasks.spawn(async move {
                run_credential_tap(
                    &relay_url,
                    &waker_keys,
                    None,
                    &owner_pubkey,
                    &agent_pubkey,
                    &mut credential_floor_store,
                    &credential_state,
                    &tap_cancel,
                )
                .await;
                TaskExit::CredentialTap {
                    pubkey: pubkey_for_exit,
                }
            });
        }

        tracing::info!(
            agent = %desired_agent.pubkey,
            owner = %desired_agent.owner_pubkey,
            "buzz-waker: roster lists a new agent; waiting for its credential"
        );
        supervised.insert(
            desired_agent.pubkey.clone(),
            SupervisedAgent {
                cancel: agent_cancel,
                source: AgentSource::Roster,
                credential_bootstrap: Some(credential_state),
                owner_pubkey: desired_agent.owner_pubkey.clone(),
            },
        );
    }

    let ready: Vec<String> = supervised
        .iter()
        .filter_map(|(pubkey, agent)| {
            agent
                .credential_bootstrap
                .as_ref()
                .and_then(|state| state.current())
                .map(|_| pubkey.clone())
        })
        .collect();
    for pubkey in ready {
        let Some(agent) = supervised.get(&pubkey) else {
            continue;
        };
        let Some(body) = agent
            .credential_bootstrap
            .as_ref()
            .and_then(|state| state.current())
        else {
            continue;
        };
        let agent_cancel = agent.cancel.clone();
        let owner_pubkey = agent.owner_pubkey.clone();

        let keys = match Keys::parse(&body.nsec) {
            Ok(keys) => keys,
            Err(error) => {
                tracing::error!(
                    agent = %pubkey,
                    %error,
                    "buzz-waker: delivered credential's nsec does not parse; tearing down this agent"
                );
                if let Some(agent) = supervised.remove(&pubkey) {
                    agent.cancel.cancel();
                }
                watch_list.remove(&pubkey);
                continue;
            }
        };
        let auth_tag = match body.auth_tag.clone().map(Tag::parse).transpose() {
            Ok(auth_tag) => auth_tag,
            Err(error) => {
                tracing::error!(
                    agent = %pubkey,
                    %error,
                    "buzz-waker: delivered credential's auth_tag does not parse; tearing down this agent"
                );
                if let Some(agent) = supervised.remove(&pubkey) {
                    agent.cancel.cancel();
                }
                watch_list.remove(&pubkey);
                continue;
            }
        };

        match spawn_agent_watch(
            relay_url,
            state_dir,
            &keys,
            auth_tag.as_ref(),
            &owner_pubkey,
            watch_list,
            agent_cancel,
            tasks,
        ) {
            Ok(()) => {
                if let Some(agent) = supervised.get_mut(&pubkey) {
                    agent.credential_bootstrap = None;
                }
                tracing::info!(agent = %pubkey, "buzz-waker: roster-discovered agent's credential arrived; now watching it");
            }
            Err(error) => {
                tracing::error!(
                    agent = %pubkey,
                    %error,
                    "buzz-waker: could not start watching a roster-discovered agent; tearing it down"
                );
                if let Some(agent) = supervised.remove(&pubkey) {
                    agent.cancel.cancel();
                }
                watch_list.remove(&pubkey);
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install the ring CryptoProvider before any wss:// feed opens. Both ring
    // and aws-lc-rs are compiled in transitively, so rustls cannot auto-select
    // one and every watch task panics on its first TLS connection without
    // this — which reads as a daemon that starts, logs its watch list, then
    // dies. Installed here rather than per-task because the provider is
    // process-level; a second install is a no-op, so the result is ignored
    // rather than unwrapped.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::registry()
        .with(fmt::layer().json().with_filter(log_env_filter()))
        .init();

    let relay_url = env_var("WAKER_RELAY_URL")?;
    let state_dir = PathBuf::from(env_var("WAKER_STATE_DIR")?);
    let agents_config_path = env_var("WAKER_AGENTS_CONFIG_PATH")?;
    let authorized_owners =
        parse_authorized_owners(&std::env::var("WAKER_OWNER_PUBKEYS").unwrap_or_default())?;
    let waker_keys = if authorized_owners.is_empty() {
        None
    } else {
        let nsec = env_var("WAKER_IDENTITY_NSEC").map_err(|_| {
            anyhow::anyhow!(
                "WAKER_OWNER_PUBKEYS is set but WAKER_IDENTITY_NSEC is not; the roster and \
                 credential taps have no identity to connect as"
            )
        })?;
        Some(Keys::parse(&nsec).map_err(|e| anyhow::anyhow!("invalid WAKER_IDENTITY_NSEC: {e}"))?)
    };

    let agent_configs = load_agents(&agents_config_path)?;

    let mut keys_by_agent: Vec<(Keys, Option<Tag>, String)> =
        Vec::with_capacity(agent_configs.len());
    let mut seen_pubkeys = HashSet::new();
    for agent in agent_configs {
        let keys = Keys::parse(&agent.nsec)
            .map_err(|e| anyhow::anyhow!("invalid nsec in WAKER_AGENTS_CONFIG_PATH: {e}"))?;
        let pubkey = normalize_pubkey(&keys.public_key().to_hex());
        if !seen_pubkeys.insert(pubkey.clone()) {
            anyhow::bail!("WAKER_AGENTS_CONFIG_PATH lists {pubkey} more than once");
        }
        let auth_tag = agent
            .auth_tag
            .map(Tag::parse)
            .transpose()
            .map_err(|e| anyhow::anyhow!("invalid auth_tag for agent {pubkey}: {e}"))?;
        let owner_pubkey = parse_owner_pubkey(&pubkey, &agent.owner_pubkey)?;
        keys_by_agent.push((keys, auth_tag, owner_pubkey));
    }

    std::fs::create_dir_all(&state_dir).map_err(|e| {
        anyhow::anyhow!(
            "could not create WAKER_STATE_DIR {}: {e}",
            state_dir.display()
        )
    })?;

    let cancel = CancellationToken::new();
    let mut tasks: JoinSet<TaskExit> = JoinSet::new();
    let watch_list = WatchList::new();
    let mut supervised: HashMap<String, SupervisedAgent> = HashMap::new();

    for (keys, auth_tag, owner_pubkey) in keys_by_agent {
        let pubkey = normalize_pubkey(&keys.public_key().to_hex());
        let agent_cancel = cancel.child_token();
        spawn_agent_watch(
            &relay_url,
            &state_dir,
            &keys,
            auth_tag.as_ref(),
            &owner_pubkey,
            &watch_list,
            agent_cancel.clone(),
            &mut tasks,
        )?;
        supervised.insert(
            pubkey,
            SupervisedAgent {
                cancel: agent_cancel,
                source: AgentSource::Config,
                credential_bootstrap: None,
                owner_pubkey,
            },
        );
    }

    let roster_state = if let Some(waker_keys) = &waker_keys {
        let roster_state = Arc::new(RosterState::new());
        let roster_floor_dir = state_dir.join("roster-floors");
        {
            let relay_url = relay_url.clone();
            let waker_keys = waker_keys.clone();
            let authorized_owners = authorized_owners.clone();
            let roster_state = Arc::clone(&roster_state);
            let cancel = cancel.clone();
            tasks.spawn(async move {
                run_roster_tap(
                    &relay_url,
                    &waker_keys,
                    None,
                    &authorized_owners,
                    &roster_floor_dir,
                    &roster_state,
                    &cancel,
                )
                .await;
                TaskExit::Component("roster_tap")
            });
        }
        Some(roster_state)
    } else {
        None
    };

    // A watch task can finish on its own, outside the shutdown path: a
    // corrupt cursor makes `run_wake_loop` return immediately, and any task
    // can panic. This loop keeps running for the daemon's whole life (not
    // just a one-shot race at startup) because reconciliation and per-agent
    // teardown are now ongoing, ordinary events, not only something that
    // happens once at shutdown.
    let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let fatal: Option<anyhow::Error> = loop {
        tokio::select! {
            () = shutdown_signal() => {
                tracing::info!("buzz-waker: shutdown signal received; stopping");
                break None;
            }
            _ = ticker.tick(), if roster_state.is_some() => {
                let (Some(waker_keys), Some(roster_state)) = (&waker_keys, &roster_state) else {
                    unreachable!("ticker only fires when roster_state is Some, which only happens alongside waker_keys");
                };
                reconcile_roster(
                    &relay_url,
                    &state_dir,
                    waker_keys,
                    &authorized_owners,
                    roster_state,
                    &mut supervised,
                    &watch_list,
                    &cancel,
                    &mut tasks,
                );
            }
            result = tasks.join_next(), if !tasks.is_empty() => {
                let Some(result) = result else { continue };
                match result {
                    Err(join_error) => {
                        break Some(anyhow::anyhow!(
                            "buzz-waker: a watch task panicked: {join_error}"
                        ));
                    }
                    Ok(TaskExit::Component(name)) => {
                        if !cancel.is_cancelled() {
                            break Some(anyhow::anyhow!(
                                "buzz-waker: component {name} exited before shutdown was requested"
                            ));
                        }
                    }
                    Ok(exit) => {
                        let pubkey = match &exit {
                            TaskExit::Agent { pubkey, .. } | TaskExit::CredentialTap { pubkey } => pubkey.clone(),
                            TaskExit::Component(_) => unreachable!("handled above"),
                        };
                        let task_name: &'static str = match &exit {
                            TaskExit::Agent { task, .. } => task,
                            TaskExit::CredentialTap { .. } => "credential_tap",
                            TaskExit::Component(_) => unreachable!("handled above"),
                        };
                        let was_cancelled = supervised
                            .get(&pubkey)
                            .map(|agent| agent.cancel.is_cancelled())
                            .unwrap_or(true);
                        let source = supervised.get(&pubkey).map(|agent| agent.source);
                        match classify_exit(source, was_cancelled) {
                            ExitDisposition::Expected => {}
                            ExitDisposition::FatalDaemon => {
                                break Some(anyhow::anyhow!(
                                    "buzz-waker: {task_name} for agent {pubkey} exited before \
                                     shutdown was requested; that agent stopped being watched — \
                                     treating as fatal rather than running silently degraded"
                                ));
                            }
                            ExitDisposition::TearDownAgent => {
                                tracing::error!(
                                    agent = %pubkey,
                                    task = %task_name,
                                    "buzz-waker: a roster-discovered agent's task exited unexpectedly; \
                                     tearing down this agent only, daemon continues"
                                );
                                if let Some(agent) = supervised.remove(&pubkey) {
                                    agent.cancel.cancel();
                                }
                                watch_list.remove(&pubkey);
                            }
                        }
                    }
                }
            }
        }
    };

    cancel.cancel();

    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            tracing::error!(%error, "buzz-waker: a watch task panicked during shutdown");
        }
    }

    tracing::info!("buzz-waker: shutdown complete");

    match fatal {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_parses_the_documented_shape() {
        let owner = "a".repeat(64);
        let json = format!(
            r#"[{{"nsec": "nsec1abc", "owner_pubkey": "{owner}"}}, {{"nsec": "deadbeef", "auth_tag": ["auth", "token"], "owner_pubkey": "{owner}"}}]"#
        );
        let json = json.as_str();
        let agents: Vec<AgentConfig> = serde_json::from_str(json).expect("parses");
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].auth_tag, None);
        assert_eq!(agents[0].owner_pubkey, owner);
        assert_eq!(
            agents[1].auth_tag,
            Some(vec!["auth".to_string(), "token".to_string()])
        );
    }

    #[test]
    fn loading_an_empty_agent_list_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agents.json");
        std::fs::write(&path, "[]").expect("write");

        let result = load_agents(path.to_str().expect("utf8 path"));
        assert!(result.is_err());
    }

    #[test]
    fn loading_a_missing_file_reports_a_clear_error() {
        let result = load_agents("/nonexistent/path/agents.json");
        assert!(result.is_err());
    }

    #[test]
    fn a_valid_owner_pubkey_is_normalized_and_accepted() {
        let owner = "A".repeat(64);
        let parsed = parse_owner_pubkey("agent", &owner).expect("valid hex pubkey parses");
        assert_eq!(parsed, "a".repeat(64));
    }

    #[test]
    fn a_malformed_owner_pubkey_is_refused_before_enrollment() {
        let error = parse_owner_pubkey("agent", "not-a-key").unwrap_err();
        assert!(
            error.to_string().contains("invalid owner_pubkey"),
            "{error}"
        );
    }

    #[test]
    fn a_pin_matching_the_configured_owner_is_accepted() {
        let owner = normalize_pubkey(&"a".repeat(64));
        assert!(ensure_owner_pin_matches("agent", &owner, &owner).is_ok());
    }

    #[test]
    fn a_pin_disagreeing_with_the_configured_owner_is_refused() {
        let pinned = normalize_pubkey(&"a".repeat(64));
        let configured = normalize_pubkey(&"b".repeat(64));
        let error = ensure_owner_pin_matches("agent", &pinned, &configured).unwrap_err();
        assert!(error.to_string().contains("disagreeing owners"), "{error}");
    }

    #[test]
    fn a_valid_agent_list_round_trips_through_load_agents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agents.json");
        std::fs::write(
            &path,
            format!(
                r#"[{{"nsec": "deadbeef", "owner_pubkey": "{}"}}]"#,
                "a".repeat(64)
            ),
        )
        .expect("write");

        let agents = load_agents(path.to_str().expect("utf8 path")).expect("loads");
        assert_eq!(agents.len(), 1);
    }

    fn entry(pubkey: &str) -> RosterEntry {
        RosterEntry {
            agent_pubkey: pubkey.to_string(),
            credential_version: 1,
        }
    }

    #[test]
    fn a_config_sourced_pubkey_is_never_desired_via_roster() {
        let pubkey = "a".repeat(64);
        let owner = "b".repeat(64);
        let mut existing = HashMap::new();
        existing.insert(normalize_pubkey(&pubkey), AgentSource::Config);

        let (desired, conflicts) =
            compute_desired_roster_agents(&[(owner, vec![entry(&pubkey)])], &existing);

        assert!(desired.is_empty());
        assert!(conflicts.is_empty());
    }

    #[test]
    fn a_roster_only_pubkey_is_desired_under_its_owner() {
        let pubkey = "a".repeat(64);
        let owner = "b".repeat(64);

        let (desired, conflicts) = compute_desired_roster_agents(
            &[(owner.clone(), vec![entry(&pubkey)])],
            &HashMap::new(),
        );

        assert_eq!(desired.len(), 1);
        assert_eq!(desired[0].pubkey, normalize_pubkey(&pubkey));
        assert_eq!(desired[0].owner_pubkey, owner);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn an_already_roster_supervised_pubkey_is_still_desired_so_it_is_not_torn_down() {
        let pubkey = "a".repeat(64);
        let owner = "b".repeat(64);
        let mut existing = HashMap::new();
        existing.insert(normalize_pubkey(&pubkey), AgentSource::Roster);

        let (desired, _) =
            compute_desired_roster_agents(&[(owner, vec![entry(&pubkey)])], &existing);

        assert_eq!(
            desired.len(),
            1,
            "an existing roster-sourced agent still listed must remain desired"
        );
    }

    #[test]
    fn two_owners_claiming_the_same_pubkey_is_a_conflict_and_the_first_wins() {
        let pubkey = "a".repeat(64);
        let owner_a = "b".repeat(64);
        let owner_b = "c".repeat(64);

        let (desired, conflicts) = compute_desired_roster_agents(
            &[
                (owner_a.clone(), vec![entry(&pubkey)]),
                (owner_b, vec![entry(&pubkey)]),
            ],
            &HashMap::new(),
        );

        assert_eq!(desired.len(), 1);
        assert_eq!(
            desired[0].owner_pubkey, owner_a,
            "the first owner seen wins"
        );
        assert_eq!(conflicts, vec![normalize_pubkey(&pubkey)]);
    }

    #[test]
    fn a_pubkey_missing_from_every_roster_is_not_desired() {
        let (desired, conflicts) = compute_desired_roster_agents(&[], &HashMap::new());
        assert!(desired.is_empty());
        assert!(conflicts.is_empty());
    }

    #[test]
    fn a_cancelled_exit_is_always_expected_regardless_of_source() {
        assert_eq!(
            classify_exit(Some(AgentSource::Config), true),
            ExitDisposition::Expected
        );
        assert_eq!(
            classify_exit(Some(AgentSource::Roster), true),
            ExitDisposition::Expected
        );
        assert_eq!(classify_exit(None, true), ExitDisposition::Expected);
    }

    #[test]
    fn an_unsolicited_config_exit_is_fatal() {
        assert_eq!(
            classify_exit(Some(AgentSource::Config), false),
            ExitDisposition::FatalDaemon
        );
    }

    #[test]
    fn an_unsolicited_exit_of_an_untracked_pubkey_is_fatal() {
        // `None` means the exiting task's pubkey has no entry in
        // `supervised` at all — never true for a real roster-sourced agent
        // (removal always cancels first), so this can only be an
        // accounting bug or a statically configured agent whose entry was
        // somehow lost. Treated the same as `Config`: fatal.
        assert_eq!(classify_exit(None, false), ExitDisposition::FatalDaemon);
    }

    #[test]
    fn an_unsolicited_roster_exit_tears_down_only_that_agent() {
        assert_eq!(
            classify_exit(Some(AgentSource::Roster), false),
            ExitDisposition::TearDownAgent
        );
    }
}
