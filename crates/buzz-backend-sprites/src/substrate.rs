//! The substrate seam (spec §Deploy State Machine, stated
//! substrate-neutrally): everything the reconciler needs from Fly Sprites,
//! as a trait, so the state machine is testable against a scripted fake with
//! a fake clock.
//!
//! Deliberately absent: sprite deletion and session kill. The v1 reconciler
//! makes **zero destructive substrate calls** — a dead session is replaced by
//! starting a new one, never by killing anything, and orphaned sprites are
//! the operator's boundary (`sprite destroy`). An interface that cannot
//! express a delete cannot be talked into one.

use std::time::Duration;

/// What the control plane knows about a sprite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpriteMeta {
    pub name: String,
    /// `cold` | `warm` | `running` (informational — never a classification
    /// input on its own; the probe is the truth about the harness).
    pub status: String,
    /// Flat `key=value` strings (see `naming`).
    pub labels: Vec<String>,
    /// `sprite` | `public` — asserted back to `sprite` during provision.
    pub url_auth: Option<String>,
}

/// One exec session, as the session list reports it.
///
/// `is_active` means "client attached / recently producing output", NOT
/// "process alive" (verified live) — which is why liveness classification
/// rests on the probe and treats this list as corroboration only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    pub id: String,
    pub command: String,
    pub tty: bool,
    pub is_active: bool,
}

/// Outcome of a bounded, non-TTY exec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateOutcome {
    Created(SpriteMeta),
    /// The deterministic name is taken — either our own earlier life or a
    /// raced contender; the caller re-reads and verifies before adopting.
    AlreadyExists,
    /// Structured rate limit with the server's own retry hint.
    CreationRateLimited {
        retry_after: Duration,
    },
    /// The org's concurrent-sprite cap; needs user action, not retries.
    ConcurrentLimit {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlAuth {
    Sprite,
}

impl UrlAuth {
    pub fn as_str(&self) -> &'static str {
        match self {
            UrlAuth::Sprite => "sprite",
        }
    }
}

/// A substrate failure. `message` may embed server-composed text; the
/// response path scrubs secrets before anything is printed (`observe`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubstrateError(pub String);

impl std::fmt::Display for SubstrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The reconciler's view of Fly Sprites. One implementation talks to the
/// real API (`client`); the test fake scripts expectation sequences.
#[allow(async_fn_in_trait)]
pub trait Substrate {
    /// `Ok(None)` on 404 — absent and not-ours are distinguished by the
    /// caller via label verification, not here.
    async fn get_sprite(&self, name: &str) -> Result<Option<SpriteMeta>, SubstrateError>;

    /// Create with identity labels and private URL auth stamped at birth, so
    /// the ownership fence exists from the first observable moment.
    async fn create_sprite(
        &self,
        name: &str,
        labels: &[String],
    ) -> Result<CreateOutcome, SubstrateError>;

    /// Re-assert URL privacy during provision (never on a live agent).
    async fn set_url_settings(&self, name: &str, auth: UrlAuth) -> Result<(), SubstrateError>;

    async fn list_sessions(&self, name: &str) -> Result<Vec<SessionMeta>, SubstrateError>;

    /// Bounded non-TTY exec. `stdin` bytes are streamed as WebSocket data
    /// frames — the transport for anything secret-bearing; the exec URL
    /// itself never carries values (the client has no env parameter by
    /// construction).
    async fn run(
        &self,
        name: &str,
        argv: &[String],
        stdin: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<ExecResult, SubstrateError>;

    /// Spawn the launcher in a detachable TTY session, capture the session id
    /// from the server's `session_info` frame, and disconnect. The process
    /// keeps running detached (verified live).
    async fn start_detached(
        &self,
        name: &str,
        argv: &[String],
        dir: &str,
    ) -> Result<String, SubstrateError>;

    /// Sleep — on the fake, this advances the fake clock instead.
    async fn sleep(&self, duration: Duration);

    /// Time since this operation began, against the same clock `sleep` moves.
    fn elapsed(&self) -> Duration;
}
