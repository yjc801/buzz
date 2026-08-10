//! [`RealWakeEffects`] — the production [`WakeEffects`] implementation.
//!
//! Wires [`crate::attempt::run_wake_attempt`] to real state: the presence tap
//! for `presence()` and `heartbeat()`, this daemon's own watch list for
//! `confirm_author_not_known_agent()`, and the system clock for `now_ms()`.
//!
//! # `start_managed_agent` is deliberately not implemented
//!
//! The provider deploy wire protocol — actually starting a sprite / container
//! / VM running the agent's harness — is out of scope for this build (see
//! `PLANS/BUZZ_WAKER_DESIGN.md` §7's open decisions, and the task that
//! produced this module: bundle transport and the deploy protocol are
//! explicitly deferred). Every real wake attempt therefore runs the full
//! decision sequence — presence, liveness proof, author re-check — and then
//! fails at the one step this crate cannot yet perform, reported as
//! [`crate::attempt::WakeOutcome::DeployFailed`] with a clearly logged reason.
//! This is intentional and must not be papered over with a fake success: a
//! stubbed "deploy" that returns `Ok(())` would make every wake look healthy
//! while waking nothing.
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
use crate::decide::normalize_pubkey;
use crate::presence_feed::{PresenceError, PresenceState};
use buzz_core::PresenceStatus;
use tokio_util::sync::CancellationToken;

/// Errors [`RealWakeEffects`] can raise.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum EffectsError {
    /// The presence tap has not resolved a reading yet — see
    /// [`crate::presence_feed::PresenceError`].
    #[error("presence unavailable: {0}")]
    Presence(#[from] PresenceError),

    /// The provider deploy wire protocol is not implemented in this build.
    /// See the module note.
    #[error(
        "provider deploy is not implemented in this build of buzz-waker \
         (bundle transport and the deploy wire protocol are out of scope); \
         refusing to claim a wake that cannot actually start anything"
    )]
    DeployNotImplemented,
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
    pub fn new(
        presence_state: Arc<PresenceState>,
        watch_list: Arc<[String]>,
        trigger_author: &str,
        cancel: CancellationToken,
        on_deployed: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            presence_state,
            watch_list,
            trigger_author: normalize_pubkey(trigger_author),
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
        tracing::error!(
            author = %self.trigger_author,
            "buzz-waker: wake attempt reached the deploy step, but the provider \
             deploy wire protocol is not implemented in this build — reporting \
             DeployFailed rather than a fake success. See effects.rs's module \
             doc: bundle transport and the deploy protocol are out of scope for \
             this daemon build."
        );
        Err(EffectsError::DeployNotImplemented)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> Arc<PresenceState> {
        Arc::new(PresenceState::new())
    }

    #[tokio::test]
    async fn an_unresolved_presence_tap_reports_unavailable() {
        let effects = RealWakeEffects::new(
            state(),
            Arc::from(vec![]),
            "aa".repeat(32).as_str(),
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
        let effects = RealWakeEffects::new(
            presence_state,
            Arc::from(vec![]),
            "aa".repeat(32).as_str(),
            CancellationToken::new(),
            || {},
        );

        assert_eq!(effects.presence().await, Ok(Some(PresenceStatus::Online)));
    }

    #[tokio::test]
    async fn an_author_on_the_watch_list_is_rejected() {
        let watched = "bb".repeat(32);
        let effects = RealWakeEffects::new(
            state(),
            Arc::from(vec![watched.clone()]),
            &watched,
            CancellationToken::new(),
            || {},
        );

        assert_eq!(effects.confirm_author_not_known_agent().await, Ok(false));
    }

    #[tokio::test]
    async fn an_author_off_the_watch_list_is_confirmed_clear() {
        let effects = RealWakeEffects::new(
            state(),
            Arc::from(vec!["bb".repeat(32)]),
            "cc".repeat(32).as_str(),
            CancellationToken::new(),
            || {},
        );

        assert_eq!(effects.confirm_author_not_known_agent().await, Ok(true));
    }

    #[tokio::test]
    async fn the_watch_list_comparison_is_case_insensitive() {
        let watched = "BB".repeat(32);
        let effects = RealWakeEffects::new(
            state(),
            Arc::from(vec![normalize_pubkey(&watched)]),
            &watched,
            CancellationToken::new(),
            || {},
        );

        assert_eq!(effects.confirm_author_not_known_agent().await, Ok(false));
    }

    #[tokio::test]
    async fn start_managed_agent_always_reports_the_unimplemented_seam() {
        let effects = RealWakeEffects::new(
            state(),
            Arc::from(vec![]),
            "aa".repeat(32).as_str(),
            CancellationToken::new(),
            || {},
        );

        let result = effects.start_managed_agent().await;
        assert!(matches!(result, Err(EffectsError::DeployNotImplemented)));
    }

    #[tokio::test]
    async fn on_deployed_invokes_the_supplied_callback() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_clone = called.clone();
        let effects = RealWakeEffects::new(
            state(),
            Arc::from(vec![]),
            "aa".repeat(32).as_str(),
            CancellationToken::new(),
            move || called_clone.store(true, std::sync::atomic::Ordering::SeqCst),
        );

        effects.on_deployed();
        assert!(called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_cancelled_token_is_observed() {
        let cancel = CancellationToken::new();
        let effects = RealWakeEffects::new(
            state(),
            Arc::from(vec![]),
            "aa".repeat(32).as_str(),
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
        let effects = RealWakeEffects::new(
            state(),
            Arc::from(vec![]),
            "aa".repeat(32).as_str(),
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
