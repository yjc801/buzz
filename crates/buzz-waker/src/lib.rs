//! `buzz-waker` — the headless wake daemon for remote Buzz agents.
//!
//! A provider-backed agent exits on its own inactivity budget and its
//! substrate deliberately never restarts it, so the only way back is an
//! explicit `deploy` from something holding its credentials. Today that is
//! Buzz Desktop ([`desktop/src/features/agents/useAgentWakeOnMention.ts`]),
//! which means a mention goes unanswered whenever the desktop is closed.
//! This crate is the always-on process that takes over that job.
//!
//! Design: `PLANS/BUZZ_WAKER_DESIGN.md` (reviewed, `APPROVE`). This module
//! implements the two foundations the rest builds on:
//!
//! - [`bundle`] — the **signed launch bundle**. The desktop stays the single
//!   resolver of an agent's deploy payload and hands the waker a signed,
//!   fully-resolved artifact; the waker resolves nothing.
//! - [`floors`] — the **durable anti-rollback floors**. A signature
//!   authenticates an old bundle just as well as a current one, so accepting
//!   one has to be gated on state that survives a restart.
//!
//! Both exist because of specific review findings; each carries the gate id
//! (`G1`–`G3`) it discharges so the reason is not lost.

pub mod bundle;
pub mod cursor;
pub mod decide;
mod fence;
pub mod floors;

pub use bundle::{BundleError, LaunchBundleBody, ProviderEnvelope, SignedLaunchBundle};
pub use cursor::{Admission, Cursor, CursorError, CursorStore, Resume};
pub use decide::{
    agent_responds_to_author, compute_wake_replay_floor, event_addresses_agent,
    is_covered_by_replay_floor, select_wake_candidates, RespondTo, TriggerEvent, WakeCandidate,
};
pub use floors::{FloorError, FloorStore, Floors};

/// Seconds of overlap to subtract from the persisted cursor when re-issuing a
/// REQ after a reconnect — **G4**.
///
/// Twice the relay's accepted clock drift, and it has to be: `since` compares
/// `created_at` directly, so a future-dated event can advance a timestamp
/// cursor by the full drift, and an event the relay then legitimately accepts
/// can be backdated by the full drift again. Anything less can step over that
/// second event and lose the mention.
///
/// Derived from [`buzz_core::relay::MAX_TIMESTAMP_DRIFT_SECS`] rather than
/// written as its own number, so it cannot drift away from the bound it is
/// defined against.
pub const RECONNECT_OVERLAP_SECS: u64 = 2 * buzz_core::relay::MAX_TIMESTAMP_DRIFT_SECS;

/// Age at which a recovered event can no longer be *guaranteed* to reach the
/// agent a wake starts — **G4**.
///
/// The harness clamps its replay floor to
/// `startup_watermark - REPLAY_FLOOR_MAX_AGE_SECS`
/// (`apply_replay_floor`, `crates/buzz-acp/src/lib.rs`), and that watermark is
/// captured *after* the wake pipeline runs, not when the waker decides to wake.
/// So the whole pipeline budget is spent between this decision and the
/// comparison the bound is actually made against, and only what is left over
/// is available to the event's age. Written as that subtraction rather than as
/// its value, because the value is a consequence of the two bounds and not a
/// number in its own right — and because the subtraction refuses to compile if
/// the budget ever grows past the bound it is taken out of.
///
/// It comes out at the relay's accepted past skew — which is the point. That
/// is the design's construction (`PLANS/BUZZ_WAKER_DESIGN.md` §5): a *live*
/// trigger is at most that old when it arrives, so live wakes are covered by
/// definition and only recovery can exceed this. Every second of waker downtime
/// spends the margin directly.
pub const WAKE_DELIVERABLE_AGE_SECS: u64 = buzz_core::relay::REPLAY_FLOOR_MAX_AGE_SECS
    - buzz_core::relay::WAKE_PIPELINE_LATENCY_BUDGET_SECS;

#[cfg(test)]
mod overlap_tests {
    /// Not a tautology: it pins the *relationship*, so a future change to the
    /// relay's drift bound moves this in lockstep instead of silently
    /// invalidating the reconnect window.
    #[test]
    fn the_overlap_is_twice_the_relay_drift_bound() {
        assert_eq!(
            super::RECONNECT_OVERLAP_SECS,
            2 * buzz_core::relay::MAX_TIMESTAMP_DRIFT_SECS
        );
    }

    /// The deliverable age is what the floor bound has left after the wake
    /// pipeline is paid for, and the design says that must land on the relay's
    /// past skew. If a future change to either bound breaks that equality, the
    /// claim "a live trigger is always deliverable" has stopped holding and
    /// this must be re-derived rather than patched.
    #[test]
    fn a_live_trigger_is_always_deliverable() {
        assert_eq!(
            super::WAKE_DELIVERABLE_AGE_SECS,
            buzz_core::relay::MAX_TIMESTAMP_DRIFT_SECS,
            "the floor bound must leave exactly the relay's past skew"
        );
    }
}
