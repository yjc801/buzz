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
mod fence;
pub mod floors;

pub use bundle::{BundleError, LaunchBundleBody, ProviderEnvelope, SignedLaunchBundle};
pub use cursor::{Admission, Cursor, CursorError, CursorStore, Resume};
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
}
