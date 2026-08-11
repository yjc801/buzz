//! [`RealWakeEffects`] — the production [`WakeEffects`] implementation.
//!
//! Wires [`crate::attempt::run_wake_attempt`] to real state: the presence tap
//! for `presence()` and `heartbeat()`, this daemon's own watch list for
//! `confirm_author_not_known_agent()`, the system clock for `now_ms()`, and
//! the provider deploy wire protocol (`buzz-provider-deploy`, shared with the
//! desktop app) for `start_managed_agent()`.
//!
//! # `start_managed_agent` needs a launch bundle it is not yet given
//!
//! The deploy call itself — stage the provider binary, verify it against the
//! bundle's pinned digest (**G1**), negotiate, invoke — is implemented and
//! shared with the desktop app via `buzz-provider-deploy`. What this crate
//! still cannot do is *obtain* a [`crate::bundle::LaunchBundleBody`] at
//! runtime: bundle transport (how a signed bundle reaches this daemon process)
//! is a separate, explicitly deferred task (see
//! `PLANS/BUZZ_WAKER_DESIGN.md` §7). Until that lands, every
//! [`RealWakeEffects`] is constructed with `bundle: None`, and a real wake
//! attempt runs the full decision sequence — presence, liveness proof, author
//! re-check — and then fails at the deploy step, reported as
//! [`crate::attempt::WakeOutcome::DeployFailed`] with a clearly logged reason.
//! This is intentional and must not be papered over with a fake success: a
//! stubbed "deploy" that returns `Ok(())` would make every wake look healthy
//! while waking nothing.
//!
//! # The generation nonce is not implemented
//!
//! The bundle doc (`crate::bundle`, `PLANS/BUZZ_WAKER_DESIGN.md` §3) says the
//! waker substitutes two wake-specific values into the bundle's `agent_json`
//! before executing: `BUZZ_ACP_REPLAY_FLOOR` (implemented below, mirroring the
//! desktop's own `apply_wake_replay_floor`) and "the generation nonce". No
//! concrete contract for that second value — an env var name, its shape, what
//! consumes it — exists anywhere in this codebase yet, so it is not invented
//! here. Left as an open follow-up rather than guessed at.
//!
//! # The known-agent baseline is this daemon's own watch list
//!
//! `confirm_author_not_known_agent` is documented (`WakeEffects`) as a fresh
//! re-check against a known-agent set, because the synchronous baseline
//! `select_wake_candidates` filtered against can be minutes stale. The
//! desktop's version re-checks the full managed-agent roster (local ∪
//! relay-registered). This daemon has no such roster — it only knows the
//! agents it was configured to watch — so its baseline is that watch list.
//! Documented in `PLANS/BUZZ_WAKER_DESIGN.md` as an accepted gap: an author
//! that is a *managed agent this daemon does not watch* is not caught here.
//! It is still caught by the synchronous baseline at admission time whenever
//! that baseline is populated the same way; the re-check only narrows the
//! window, and narrowing it to "this daemon's own agents" is strictly better
//! than not re-checking at all.

use std::sync::Arc;

use crate::attempt::{HeartbeatObservation, WakeEffects};
use crate::bundle::LaunchBundleBody;
use crate::decide::normalize_pubkey;
use crate::presence_feed::{PresenceError, PresenceState};
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

    /// This attempt has no signed launch bundle to deploy from. Expected
    /// until bundle transport is wired in — see the module note.
    #[error(
        "no launch bundle available for this agent; refusing to claim a wake \
         that cannot actually start anything (bundle transport is not yet \
         wired into this daemon build)"
    )]
    NoBundle,

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
    /// This daemon's full watch list, normalized. Used only by
    /// `confirm_author_not_known_agent`; see the module note on why this is
    /// the accepted baseline rather than a full managed-agent roster.
    watch_list: Arc<[String]>,
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
        watch_list: Arc<[String]>,
        trigger_author: &str,
        trigger_created_at: u64,
        bundle: Option<Arc<LaunchBundleBody>>,
        cancel: CancellationToken,
        on_deployed: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            presence_state,
            watch_list,
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
        Ok(self.presence_state.snapshot()?)
    }

    async fn confirm_author_not_known_agent(&self) -> Result<bool, Self::Error> {
        // `Ok(true)` means "confirmed not a known agent" — see the trait doc.
        // Every entry in the watch list is, by definition, a known agent this
        // daemon manages, so the author is clear exactly when it matches
        // none of them.
        Ok(!self
            .watch_list
            .iter()
            .any(|watched| watched == &self.trigger_author))
    }

    async fn start_managed_agent(&self) -> Result<(), Self::Error> {
        let Some(bundle) = self.bundle.clone() else {
            tracing::error!(
                author = %self.trigger_author,
                "buzz-waker: wake attempt reached the deploy step with no launch \
                 bundle available — reporting DeployFailed rather than a fake \
                 success. Bundle transport is not yet wired into this daemon build."
            );
            return Err(EffectsError::NoBundle);
        };

        let binary = buzz_provider_deploy::resolve_provider_binary(&bundle.provider.provider_id)
            .map_err(EffectsError::ProviderUnresolved)?;

        // Substitute the one wake-specific value this crate implements — see
        // the module note on the generation nonce, which it does not.
        let mut agent_json = bundle.agent_json.clone();
        agent_json["launch"]["policy_env"][REPLAY_FLOOR_ENV_KEY] =
            serde_json::Value::String(self.trigger_created_at.to_string());

        let provider_config = bundle.provider.provider_config.clone();
        let expected_digest = bundle.provider.provider_binary_sha256.clone();

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
        Ok(())
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
        watch_list: Arc<[String]>,
        trigger_author: &str,
        bundle: Option<Arc<LaunchBundleBody>>,
        cancel: CancellationToken,
        on_deployed: impl Fn() + Send + Sync + 'static,
    ) -> RealWakeEffects {
        RealWakeEffects::new(
            presence_state,
            watch_list,
            trigger_author,
            1_000,
            bundle,
            cancel,
            on_deployed,
        )
    }

    #[tokio::test]
    async fn an_unresolved_presence_tap_reports_unavailable() {
        let effects = effects_with(
            state(),
            Arc::from(vec![]),
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
            Arc::from(vec![]),
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
            Arc::from(vec![watched.clone()]),
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
            Arc::from(vec!["bb".repeat(32)]),
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
            Arc::from(vec![normalize_pubkey(&watched)]),
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
            Arc::from(vec![]),
            "aa".repeat(32).as_str(),
            None,
            CancellationToken::new(),
            || {},
        );

        let result = effects.start_managed_agent().await;
        assert!(matches!(result, Err(EffectsError::NoBundle)));
    }

    /// A bundle whose `provider_id` resolves to nothing on `PATH` fails at
    /// resolution, before any process is spawned.
    #[tokio::test]
    async fn start_managed_agent_with_an_unresolvable_provider_fails_to_resolve() {
        let bundle = Arc::new(LaunchBundleBody {
            agent_pubkey: "a".repeat(64),
            agent_json: serde_json::json!({"launch": {"policy_env": {}}}),
            provider: ProviderEnvelope {
                provider_id: "zzz-nonexistent-test-provider".to_string(),
                provider_config: serde_json::json!({}),
                provider_binary_sha256: "b".repeat(64),
            },
            bundle_version: 1,
            issued_at: 0,
            expires_at: u64::MAX,
            owner_only_access: true,
        });
        let effects = effects_with(
            state(),
            Arc::from(vec![]),
            "aa".repeat(32).as_str(),
            Some(bundle),
            CancellationToken::new(),
            || {},
        );

        let result = effects.start_managed_agent().await;
        assert!(matches!(result, Err(EffectsError::ProviderUnresolved(_))));
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
            Arc::from(vec![]),
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
            Arc::from(vec![]),
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
            Arc::from(vec![]),
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
