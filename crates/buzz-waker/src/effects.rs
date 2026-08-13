//! [`RealWakeEffects`] — the production [`WakeEffects`] implementation.
//!
//! Wires [`crate::attempt::run_wake_attempt`] to real state: the presence tap
//! for `presence()` and `heartbeat()`, this daemon's own watch list for
//! `confirm_author_not_known_agent()`, the system clock for `now_ms()`, and
//! the provider deploy wire protocol (`buzz-provider-deploy`, shared with the
//! desktop app) for `start_managed_agent()`.
//!
//! # `start_managed_agent`
//!
//! The deploy call itself — bind the bundle to the agent this attempt
//! watches, recheck the bundle has not expired since activation, stage the
//! provider binary, verify it against the bundle's pinned digest (**G1**),
//! negotiate, invoke — is implemented and shared with the desktop app via
//! `buzz-provider-deploy`. The bundle it deploys comes from
//! [`crate::bundle_feed::BundleState`], written by
//! [`crate::bundle_feed::run_bundle_tap`] (§11, `PLANS/BUZZ_WAKER_DESIGN.md`)
//! — read fresh at the moment each attempt reaches this step, per
//! `wake_loop`'s own doc on why it isn't captured once at loop-construction
//! time. A wake attempt whose agent has never had a bundle delivered and
//! admitted yet — a fresh daemon before the tap's first delivery, or an
//! agent never enrolled for remote wake at all — fails at the deploy step,
//! reported as [`crate::attempt::WakeOutcome::DeployFailed`] with a clearly
//! logged reason. This is intentional and must not be papered over with a
//! fake success: a stubbed "deploy" that returns `Ok(())` would make every
//! wake look healthy while waking nothing.
//!
//! # The generation nonce doesn't apply here
//!
//! An earlier draft of the bundle doc (`crate::bundle`) named a second
//! wake-specific substitution alongside `BUZZ_ACP_REPLAY_FLOOR` (implemented
//! below, mirroring the desktop's own `apply_wake_replay_floor`): a
//! "generation nonce". That term traces back to an abandoned design (a
//! bearer-secret HTTP `/wake` endpoint) that needed nonce-based replay
//! binding; it was never re-derived for what actually got built here.
//! Replay protection for the signed launch bundle this crate deploys comes
//! from `bundle_version` (`FloorStore::admit`), `issued_at`/`expires_at`, and
//! the owner's signature instead — none of which need a nonce. There is
//! exactly one wake-specific substitution.
//!
//! # The known-agent baseline is this daemon's own watch list
//!
//! `confirm_author_not_known_agent` is documented (`WakeEffects`) as a fresh
//! re-check against a known-agent set, because the synchronous baseline
//! `select_wake_candidates` filtered against can be minutes stale. The
//! desktop's version re-checks the full managed-agent roster (local ∪
//! relay-registered). This daemon has no such roster — it only knows the
//! agents it was configured to watch — so its baseline is that watch list,
//! held in a [`crate::watch_list::WatchList`] and read live rather than
//! snapshotted, since the dynamic supervisor (`PLANS/BUZZ_WAKER_DESIGN.md`
//! §12 build order step 3) can add or remove a watched agent at any time —
//! see that module's own doc for why a frozen snapshot would let a
//! roster-added agent's mention wake another agent undetected. Still
//! documented as an accepted gap: an author that is a *managed agent this
//! daemon does not watch* is not caught here. It is still caught by the
//! synchronous baseline at admission time whenever that baseline is
//! populated the same way; the re-check only narrows the window, and
//! narrowing it to "this daemon's own agents" is strictly better than not
//! re-checking at all.

use std::sync::Arc;

use crate::attempt::{HeartbeatObservation, WakeEffects};
use crate::bundle::LaunchBundleBody;
use crate::decide::normalize_pubkey;
use crate::presence_feed::{PresenceError, PresenceState};
use crate::watch_list::WatchList;
use buzz_core::PresenceStatus;
use tokio_util::sync::CancellationToken;

/// The launch contract's wake-replay-floor key — mirrors the desktop's own
/// `apply_wake_replay_floor` (`desktop/src-tauri/src/commands/agents_deploy.rs`)
/// exactly, since both write into the same provider-consumed shape.
const REPLAY_FLOOR_ENV_KEY: &str = "BUZZ_ACP_REPLAY_FLOOR";

/// Errors [`RealWakeEffects`] can raise.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum EffectsError {
    /// The presence tap has not resolved a reading yet — see
    /// [`crate::presence_feed::PresenceError`].
    #[error("presence unavailable: {0}")]
    Presence(#[from] PresenceError),

    /// This attempt's agent has no admitted launch bundle yet — the tap
    /// hasn't delivered a first one since this daemon started, or the owner
    /// has never issued one for this agent.
    #[error(
        "no launch bundle available for this agent; refusing to claim a wake \
         that cannot actually start anything"
    )]
    NoBundle,

    /// The bundle authorizes a different agent than the one this attempt is
    /// watching.
    ///
    /// Checked before any provider resolution or process spawn: a bundle
    /// transport bug that routes, caches, or restores agent B's valid,
    /// owner-signed bundle into agent A's wake loop must not deploy B's
    /// secret-bearing payload while this attempt believes it is waking A.
    #[error("launch bundle authorizes agent {found}, not the watched agent {expected}")]
    AgentMismatch {
        /// The pubkey this attempt is scoped to.
        expected: String,
        /// The pubkey the bundle actually authorizes.
        found: String,
    },

    /// The bundle's validity window has lapsed since it was verified and
    /// activated into this daemon's `WakeLoopConfig`.
    ///
    /// `SignedLaunchBundle::verify` only checks expiry once, at activation
    /// time — the body then rests in memory indefinitely. Rechecked here,
    /// immediately before any external effect, so a bundle that expires
    /// while resident cannot still launch days later with a revoked
    /// credential or access policy.
    #[error("launch bundle expired at {expires_at} (now {now})")]
    BundleExpired {
        /// The bundle's expiry, unix seconds.
        expires_at: u64,
        /// The current time, unix seconds.
        now: u64,
    },

    /// The bundle's `provider_id` did not resolve to a discovered binary.
    #[error("provider binary unresolved: {0}")]
    ProviderUnresolved(String),

    /// The provider deploy call itself failed — includes a pinned-digest
    /// mismatch (**G1**), a protocol negotiation failure, or a non-`ok`
    /// provider response. Never contains a credential: `buzz-provider-deploy`
    /// redacts before returning.
    #[error("provider deploy failed: {0}")]
    Deploy(String),
}

/// The production [`WakeEffects`] for one wake attempt.
///
/// Scoped to **one triggering event's author** and **one watched agent's**
/// presence tap — a fresh instance is built per attempt by
/// [`crate::wake_loop::run_wake_loop`], carrying that attempt's own
/// generation-cancellation token so a shutdown mid-attempt is observed by
/// every effect method rather than only at the loop's own select points.
pub struct RealWakeEffects {
    /// The presence tap shared with every attempt for this agent — one tap
    /// per watched agent, not per attempt.
    presence_state: Arc<PresenceState>,
    /// This daemon's live watch list. Used only by
    /// `confirm_author_not_known_agent`; see the module note on why this is
    /// the accepted baseline rather than a full managed-agent roster, and
    /// `crate::watch_list`'s own doc on why it must be read live rather than
    /// snapshotted once an agent can be added or removed at runtime.
    watch_list: WatchList,
    /// The pubkey of the agent this attempt is scoped to — this daemon's own
    /// watched identity, never derived from the bundle. Compared against
    /// `bundle.agent_pubkey` before any deploy, so a bundle transport bug
    /// that hands this attempt another agent's validly-signed bundle is
    /// refused rather than deployed.
    expected_agent_pubkey: String,
    /// The pubkey that authored the triggering event, normalized once at
    /// construction so every re-check compares like with like.
    trigger_author: String,
    /// `created_at` (unix seconds) of the triggering event — written into
    /// the deploy payload's `BUZZ_ACP_REPLAY_FLOOR` so a cold-started harness
    /// resubscribes far enough back to catch the mention that woke it.
    trigger_created_at: u64,
    /// This attempt's launch bundle, if one is available. `None` until
    /// bundle transport is wired into this daemon — see the module note.
    bundle: Option<Arc<LaunchBundleBody>>,
    /// Fires on daemon shutdown. Deliberately **not** tied to the mention
    /// feed's own connection lifecycle: a wake attempt does not touch that
    /// socket, so a feed reconnect must not cancel an attempt that is still
    /// legitimately running — only a real shutdown should.
    cancel: CancellationToken,
    /// Fires exactly once, when the attempt's deploy is accepted and it is
    /// not a reconcile — surfaces the "waking up" signal to the caller.
    on_deployed: Box<dyn Fn() + Send + Sync>,
}

impl RealWakeEffects {
    /// Build the effects for one wake attempt.
    ///
    /// `on_deployed` is invoked synchronously from
    /// [`WakeEffects::on_deployed`] — keep it cheap (a log line, a metric
    /// increment); it runs inside the attempt's own task.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        presence_state: Arc<PresenceState>,
        watch_list: WatchList,
        watched_agent_pubkey: &str,
        trigger_author: &str,
        trigger_created_at: u64,
        bundle: Option<Arc<LaunchBundleBody>>,
        cancel: CancellationToken,
        on_deployed: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            presence_state,
            watch_list,
            expected_agent_pubkey: normalize_pubkey(watched_agent_pubkey),
            trigger_author: normalize_pubkey(trigger_author),
            trigger_created_at,
            bundle,
            cancel,
            on_deployed: Box::new(on_deployed),
        }
    }
}

impl WakeEffects for RealWakeEffects {
    type Error = EffectsError;

    fn now_ms(&self) -> u64 {
        crate::presence_feed::now_ms()
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    fn heartbeat(&self) -> Option<HeartbeatObservation> {
        self.presence_state.heartbeat()
    }

    fn on_deployed(&self) {
        (self.on_deployed)();
    }

    async fn delay(&self, ms: u64) {
        tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_millis(ms)) => {}
            () = self.cancel.cancelled() => {}
        }
    }

    async fn presence(&self) -> Result<Option<PresenceStatus>, Self::Error> {
        let snapshot = self.presence_state.snapshot();
        if let Err(error) = &snapshot {
            // The attempt no longer refuses on this, so nothing downstream
            // would otherwise say it happened — and "deployed without knowing
            // whether the agent was already up" is exactly the kind of thing
            // that must not be inferred from silence later.
            tracing::warn!(
                agent = %self.expected_agent_pubkey,
                %error,
                "buzz-waker: presence unresolved; treating liveness as unproven and \
                 reconciling through the idempotent deploy"
            );
        }
        Ok(snapshot?)
    }

    async fn confirm_author_not_known_agent(&self) -> Result<bool, Self::Error> {
        // `Ok(true)` means "confirmed not a known agent" — see the trait doc.
        // Every member of the watch list is, by definition, a known agent
        // this daemon manages, so the author is clear exactly when it is not
        // currently a member. Read live (`WatchList::contains`), not from a
        // snapshot taken when this attempt was constructed — the whole point
        // of a fresh re-check is to catch a watch-list change since the
        // synchronous baseline ran.
        Ok(!self.watch_list.contains(&self.trigger_author))
    }

    async fn start_managed_agent(&self) -> Result<Option<bool>, Self::Error> {
        let Some(bundle) = self.bundle.clone() else {
            tracing::error!(
                author = %self.trigger_author,
                "buzz-waker: wake attempt reached the deploy step with no admitted \
                 launch bundle for this agent — reporting DeployFailed rather than \
                 a fake success."
            );
            return Err(EffectsError::NoBundle);
        };

        let bundle_agent = normalize_pubkey(&bundle.agent_pubkey);
        if bundle_agent != self.expected_agent_pubkey {
            tracing::error!(
                expected = %self.expected_agent_pubkey,
                found = %bundle_agent,
                "buzz-waker: launch bundle authorizes a different agent than this wake \
                 attempt is watching — refusing to deploy"
            );
            return Err(EffectsError::AgentMismatch {
                expected: self.expected_agent_pubkey.clone(),
                found: bundle_agent,
            });
        }

        // Rechecked here rather than trusted from `SignedLaunchBundle::verify`
        // — see the module note and `EffectsError::BundleExpired`.
        let now = self.now_ms() / 1000;
        if now > bundle.expires_at {
            tracing::error!(
                expires_at = bundle.expires_at,
                now,
                "buzz-waker: resident launch bundle has expired since activation — \
                 refusing to deploy"
            );
            return Err(EffectsError::BundleExpired {
                expires_at: bundle.expires_at,
                now,
            });
        }

        // Select the digest for the platform this daemon actually runs on,
        // before anything touches the filesystem. The owner authorizes a build
        // per target; a bundle naming none for this one has authorized nothing
        // here, and absence of authorization is not permission (**G1**). What
        // happens to sit on PATH is irrelevant when nothing may run.
        let target = crate::bundle::current_target_triple().ok_or_else(|| {
            EffectsError::Deploy(format!(
                "no provider build is published for this daemon's platform; \
                 the bundle authorizes {:?}",
                bundle.provider.authorized_targets()
            ))
        })?;
        let expected_digest = bundle
            .provider
            .digest_for_target(target)
            .ok_or_else(|| {
                EffectsError::Deploy(format!(
                    "launch bundle authorizes no provider build for {target}; \
                     it names {:?}",
                    bundle.provider.authorized_targets()
                ))
            })?
            .to_string();

        let binary = buzz_provider_deploy::resolve_provider_binary(&bundle.provider.provider_id)
            .map_err(EffectsError::ProviderUnresolved)?;

        // Substitute the one wake-specific value this crate implements — see
        // the module note on why there is only one.
        let mut agent_json = bundle.agent_json.clone();
        agent_json["launch"]["policy_env"][REPLAY_FLOOR_ENV_KEY] =
            serde_json::Value::String(self.trigger_created_at.to_string());

        let provider_config = bundle.provider.provider_config.clone();

        let outcome = tokio::task::spawn_blocking(move || {
            buzz_provider_deploy::provider_deploy_pinned(
                &binary,
                &agent_json,
                &provider_config,
                None,
                &expected_digest,
            )
        })
        .await
        .map_err(|error| EffectsError::Deploy(format!("deploy task panicked: {error}")))?
        .map_err(EffectsError::Deploy)?;

        tracing::info!(
            author = %self.trigger_author,
            agent_id = %outcome.agent_id,
            fresh_generation = ?outcome.fresh_generation,
            "buzz-waker: provider deploy accepted"
        );
        Ok(outcome.fresh_generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::ProviderEnvelope;

    fn state() -> Arc<PresenceState> {
        Arc::new(PresenceState::new())
    }

    #[allow(clippy::too_many_arguments)]
    fn effects_with(
        presence_state: Arc<PresenceState>,
        watch_list: WatchList,
        watched_agent_pubkey: &str,
        trigger_author: &str,
        bundle: Option<Arc<LaunchBundleBody>>,
        cancel: CancellationToken,
        on_deployed: impl Fn() + Send + Sync + 'static,
    ) -> RealWakeEffects {
        RealWakeEffects::new(
            presence_state,
            watch_list,
            watched_agent_pubkey,
            trigger_author,
            1_000,
            bundle,
            cancel,
            on_deployed,
        )
    }

    /// A bundle authorizing `agent_pubkey`, valid until `expires_at` (unix
    /// seconds).
    fn bundle_for(agent_pubkey: &str, expires_at: u64) -> Arc<LaunchBundleBody> {
        Arc::new(LaunchBundleBody {
            agent_pubkey: agent_pubkey.to_string(),
            agent_json: serde_json::json!({"launch": {"policy_env": {}}}),
            provider: ProviderEnvelope {
                provider_id: "zzz-nonexistent-test-provider".to_string(),
                provider_config: serde_json::json!({}),
                provider_binary_sha256_by_target: crate::bundle::test_digests(&"b".repeat(64)),
            },
            bundle_version: 1,
            issued_at: 0,
            expires_at,
            owner_only_access: true,
            revoked: false,
        })
    }

    #[tokio::test]
    async fn an_unresolved_presence_tap_reports_unavailable() {
        let effects = effects_with(
            state(),
            WatchList::from(vec![]),
            "aa".repeat(32).as_str(),
            "aa".repeat(32).as_str(),
            None,
            CancellationToken::new(),
            || {},
        );

        let result = effects.presence().await;
        assert!(matches!(
            result,
            Err(EffectsError::Presence(PresenceError::Unresolved))
        ));
    }

    #[tokio::test]
    async fn a_resolved_presence_tap_answers_from_the_cache() {
        let presence_state = state();
        presence_state.observe("ev1", PresenceStatus::Online, 1_000);
        let effects = effects_with(
            presence_state,
            WatchList::from(vec![]),
            "aa".repeat(32).as_str(),
            "aa".repeat(32).as_str(),
            None,
            CancellationToken::new(),
            || {},
        );

        assert_eq!(effects.presence().await, Ok(Some(PresenceStatus::Online)));
    }

    #[tokio::test]
    async fn an_author_on_the_watch_list_is_rejected() {
        let watched = "bb".repeat(32);
        let effects = effects_with(
            state(),
            WatchList::from(vec![watched.clone()]),
            "aa".repeat(32).as_str(),
            &watched,
            None,
            CancellationToken::new(),
            || {},
        );

        assert_eq!(effects.confirm_author_not_known_agent().await, Ok(false));
    }

    #[tokio::test]
    async fn an_author_off_the_watch_list_is_confirmed_clear() {
        let effects = effects_with(
            state(),
            WatchList::from(vec!["bb".repeat(32)]),
            "aa".repeat(32).as_str(),
            "cc".repeat(32).as_str(),
            None,
            CancellationToken::new(),
            || {},
        );

        assert_eq!(effects.confirm_author_not_known_agent().await, Ok(true));
    }

    #[tokio::test]
    async fn the_watch_list_comparison_is_case_insensitive() {
        let watched = "BB".repeat(32);
        let effects = effects_with(
            state(),
            WatchList::from(vec![normalize_pubkey(&watched)]),
            "aa".repeat(32).as_str(),
            &watched,
            None,
            CancellationToken::new(),
            || {},
        );

        assert_eq!(effects.confirm_author_not_known_agent().await, Ok(false));
    }

    #[tokio::test]
    async fn start_managed_agent_without_a_bundle_reports_no_bundle() {
        let effects = effects_with(
            state(),
            WatchList::from(vec![]),
            "aa".repeat(32).as_str(),
            "aa".repeat(32).as_str(),
            None,
            CancellationToken::new(),
            || {},
        );

        let result = effects.start_managed_agent().await;
        assert!(matches!(result, Err(EffectsError::NoBundle)));
    }

    /// A bundle whose `provider_id` resolves to nothing on `PATH` fails at
    /// resolution, before any process is spawned. Uses a bundle correctly
    /// bound to the watched agent and not expired, so resolution is the
    /// first thing that can fail.
    #[tokio::test]
    async fn start_managed_agent_with_an_unresolvable_provider_fails_to_resolve() {
        let watched = "a".repeat(64);
        let bundle = bundle_for(&watched, u64::MAX);
        let effects = effects_with(
            state(),
            WatchList::from(vec![]),
            &watched,
            "aa".repeat(32).as_str(),
            Some(bundle),
            CancellationToken::new(),
            || {},
        );

        let result = effects.start_managed_agent().await;
        assert!(matches!(result, Err(EffectsError::ProviderUnresolved(_))));
    }

    /// G-bind: a bundle authorizing a different agent than this attempt
    /// watches must be refused before any provider resolution — otherwise a
    /// bundle-transport mix-up could deploy the wrong agent's secret-bearing
    /// payload while this attempt believes it is waking its own.
    #[tokio::test]
    async fn start_managed_agent_with_a_bundle_for_a_different_agent_is_refused() {
        let watched = "a".repeat(64);
        let other_agent = "d".repeat(64);
        let bundle = bundle_for(&other_agent, u64::MAX);
        let effects = effects_with(
            state(),
            WatchList::from(vec![]),
            &watched,
            "aa".repeat(32).as_str(),
            Some(bundle),
            CancellationToken::new(),
            || {},
        );

        let result = effects.start_managed_agent().await;
        assert_eq!(
            result,
            Err(EffectsError::AgentMismatch {
                expected: watched,
                found: other_agent,
            })
        );
    }

    /// A bundle that was valid when it was verified and activated into
    /// `WakeLoopConfig` but has since sat resident past `expires_at` must be
    /// refused at deploy time, not just at activation.
    #[tokio::test]
    async fn start_managed_agent_with_an_expired_resident_bundle_is_refused() {
        let watched = "a".repeat(64);
        // Expired in 1970 relative to any real wall clock this test runs on.
        let bundle = bundle_for(&watched, 1);
        let effects = effects_with(
            state(),
            WatchList::from(vec![]),
            &watched,
            "aa".repeat(32).as_str(),
            Some(bundle),
            CancellationToken::new(),
            || {},
        );

        let result = effects.start_managed_agent().await;
        assert!(matches!(result, Err(EffectsError::BundleExpired { .. })));
    }

    /// The replay floor is written into the bundle's `agent_json` before the
    /// deploy call, at the same path the desktop's own
    /// `apply_wake_replay_floor` writes it — a mismatch here would mean a
    /// waker-started harness and a desktop-started harness read the wake
    /// floor from two different places.
    #[test]
    fn the_replay_floor_env_key_matches_the_desktop_launch_contract() {
        assert_eq!(REPLAY_FLOOR_ENV_KEY, "BUZZ_ACP_REPLAY_FLOOR");
    }

    #[tokio::test]
    async fn on_deployed_invokes_the_supplied_callback() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_clone = called.clone();
        let effects = effects_with(
            state(),
            WatchList::from(vec![]),
            "aa".repeat(32).as_str(),
            "aa".repeat(32).as_str(),
            None,
            CancellationToken::new(),
            move || called_clone.store(true, std::sync::atomic::Ordering::SeqCst),
        );

        effects.on_deployed();
        assert!(called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_cancelled_token_is_observed() {
        let cancel = CancellationToken::new();
        let effects = effects_with(
            state(),
            WatchList::from(vec![]),
            "aa".repeat(32).as_str(),
            "aa".repeat(32).as_str(),
            None,
            cancel.clone(),
            || {},
        );
        assert!(!effects.is_cancelled());
        cancel.cancel();
        assert!(effects.is_cancelled());
    }

    #[tokio::test]
    async fn delay_returns_early_when_cancelled() {
        let cancel = CancellationToken::new();
        let effects = effects_with(
            state(),
            WatchList::from(vec![]),
            "aa".repeat(32).as_str(),
            "aa".repeat(32).as_str(),
            None,
            cancel.clone(),
            || {},
        );
        cancel.cancel();

        let start = std::time::Instant::now();
        effects.delay(60_000).await;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "a cancelled token must cut the delay short"
        );
    }
}
