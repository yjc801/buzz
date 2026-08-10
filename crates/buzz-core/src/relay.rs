//! Canonical relay identities shared by runtime components.

use thiserror::Error;
use url::{Host, Url};

/// How far an event's `created_at` may sit from server time and still be
/// accepted, in seconds (±15 minutes).
///
/// This is **relay ingest policy**, and it is defined here rather than in
/// `buzz-relay` because two other components have to reason about the same
/// number and cannot import the relay:
///
/// - `buzz-acp` derives its replay-floor age cap from it, so that a wake
///   deploy can still hand a fresh harness a floor reaching the oldest
///   trigger the relay would have accepted;
/// - `buzz-waker` sizes its reconnect overlap at `2 ×` this value, because a
///   future-dated event can advance a timestamp cursor by the full drift and
///   an accepted backdated event can then land the full drift behind it.
///
/// Those were previously independent `900` literals in separate crates with
/// nothing tying them together, so changing the relay's tolerance would have
/// silently invalidated the other two. One definition removes the question.
pub const MAX_TIMESTAMP_DRIFT_SECS: u64 = 900;

/// Time a wake pipeline can legitimately spend between trigger delivery and
/// the woken harness capturing its startup watermark, as the SUM of the
/// enforced bounds along the path — each term is a real timeout or window in
/// code, not an estimate, because any single term assigned to the whole
/// pipeline silently under-budgets the rest:
///
/// - the wake decision's stale-online liveness evidence window
///   (`WAKE_LIVE_EVIDENCE_ATTEMPTS × POLL` = 135s, `agentWake.ts`);
/// - the post-offline teardown fence (`REMOTE_POST_OFFLINE_GRACE_MS` = 10s);
/// - the provider `info` probe invocation timeout (10s, `backend.rs`);
/// - the provider `deploy` invocation timeout (600s, `backend.rs`);
/// - margin for the pre-deploy author fetch, provider-internal VM
///   provisioning after the deploy call returns, and harness boot up to the
///   watermark capture.
pub const WAKE_PIPELINE_LATENCY_BUDGET_SECS: u64 = 135 + 10 + 10 + 600 + 300;

/// Max age of an externally supplied replay floor, relative to the woken
/// harness's startup watermark.
///
/// Bounds how much history a stale or corrupted `BUZZ_ACP_REPLAY_FLOOR` can
/// force back into the first REQ; a floor older than this is clamped to the
/// bound rather than ignored, so a slow deploy still replays as much of the
/// missed window as the bound allows.
///
/// Sized as the relay's accepted past skew PLUS the wake pipeline latency
/// budget: a trigger accepted at the relay's maximum age has aged further by
/// every second of fence/evidence/deploy/boot latency, and a cap equal to the
/// skew alone would advance the watermark past the very trigger that caused
/// the start.
///
/// Defined here rather than in `buzz-acp` because it is the contract between
/// two crates, not one crate's internal: the harness *enforces* it, and
/// `buzz-waker` needs the same number to know when a recovery gap has grown
/// past what a woken agent could still be shown — beyond this, waking the
/// agent would answer nobody, so the waker raises an operational failure
/// instead of pretending the mention was delivered.
pub const REPLAY_FLOOR_MAX_AGE_SECS: u64 =
    MAX_TIMESTAMP_DRIFT_SECS + WAKE_PIPELINE_LATENCY_BUDGET_SECS;

/// Tag a client puts in its signed NIP-42 AUTH event to request a restricted
/// connection class.
///
/// The AUTH event is the only place connection-scoped intent can be stated and
/// trusted, because the Schnorr signature covers the tags.
pub const CONNECTION_CLASS_TAG: &str = "class";

/// The default connection class: may publish, and bears presence. What every
/// connection gets when no [`CONNECTION_CLASS_TAG`] is sent.
pub const CONNECTION_CLASS_INTERACTIVE: &str = "interactive";

/// A connection that may subscribe and receive but may not publish, and does
/// not count as its principal being present.
///
/// For processes that hold a connection **as** an agent without **being** it —
/// the wake daemon. See `docs/remote-agents.md` §I3(c).
pub const CONNECTION_CLASS_READ_ONLY: &str = "read-only";

/// Prefix the relay uses to confirm an applied connection class in the AUTH
/// `OK` message, e.g. `class: read-only`.
///
/// The confirmation exists so a client can tell a relay that *applied* the
/// class from one that has never heard of it — an older relay ignores the tag
/// and answers `OK true` with an empty message, which would silently hand a
/// watcher the fully-capable connection it asked not to have.
///
/// Defined here for the same reason as the drift bound above: the relay writes
/// this string and the client compares against it, and as two independent
/// literals in two crates nothing would keep them equal.
pub const CONNECTION_CLASS_CONFIRMATION_PREFIX: &str = "class: ";

/// Errors returned while canonicalizing a relay URL for runtime identity.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum NormalizeRelayUrlError {
    /// The input is not a valid URL.
    #[error("invalid relay URL: {0}")]
    InvalidUrl(String),
    /// Relay sockets must use WebSocket schemes.
    #[error("relay URL scheme must be ws or wss")]
    InvalidScheme,
    /// Relay identity never includes user credentials.
    #[error("relay URL must not contain credentials")]
    Credentials,
    /// Relay identity never includes a fragment.
    #[error("relay URL must not contain a fragment")]
    Fragment,
    /// A relay URL requires a host.
    #[error("relay URL must contain a host")]
    MissingHost,
}

/// Canonicalize a WebSocket relay URL for use as a runtime identity key.
///
/// This is the sole normalizer for `(agent, relay)` process identity. It keeps
/// the WebSocket scheme, lowercases DNS hosts, folds all loopback spellings to
/// `127.0.0.1`, removes default ports and a root slash, and preserves non-root
/// paths and queries. It deliberately is **not** the NIP-42 AUTH comparison
/// helper in `buzz-auth`: AUTH validation is a security boundary with narrower
/// equivalence rules and must not be widened by runtime-key canonicalization.
///
/// Connection code may retain the configured URL; this canonical form is for
/// identity, receipts, status and deduplication.
pub fn normalize_relay_url(raw: &str) -> Result<String, NormalizeRelayUrlError> {
    let mut url = Url::parse(raw.trim())
        .map_err(|error| NormalizeRelayUrlError::InvalidUrl(error.to_string()))?;
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err(NormalizeRelayUrlError::InvalidScheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(NormalizeRelayUrlError::Credentials);
    }
    if url.fragment().is_some() {
        return Err(NormalizeRelayUrlError::Fragment);
    }

    let host = url.host().ok_or(NormalizeRelayUrlError::MissingHost)?;
    let loopback = match host {
        Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
    };
    if loopback {
        url.set_host(Some("127.0.0.1"))
            .map_err(|_| NormalizeRelayUrlError::MissingHost)?;
    } else if let Host::Domain(domain) = host {
        let lowercase = domain.to_ascii_lowercase();
        url.set_host(Some(&lowercase))
            .map_err(|_| NormalizeRelayUrlError::MissingHost)?;
    }

    let default_port = match url.scheme() {
        "ws" => Some(80),
        "wss" => Some(443),
        _ => None,
    };
    if url.port() == default_port {
        url.set_port(None)
            .map_err(|_| NormalizeRelayUrlError::InvalidScheme)?;
    }
    if url.path() == "/" {
        url.set_path("");
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_spellings_have_one_identity() {
        let ipv6 = normalize_relay_url("wss://[::1]/").unwrap();
        let ipv4 = normalize_relay_url("wss://127.0.0.1/").unwrap();
        let localhost = normalize_relay_url("wss://localhost/").unwrap();
        assert_eq!(ipv6, ipv4);
        assert_eq!(ipv4, localhost);
        assert_eq!(localhost, "wss://127.0.0.1");
    }

    #[test]
    fn canonicalizes_only_identity_equivalences() {
        assert_eq!(
            normalize_relay_url(" WSS://Relay.Example:443/ ").unwrap(),
            "wss://relay.example"
        );
        assert_eq!(
            normalize_relay_url("ws://relay.example:8080/community/?x=1").unwrap(),
            "ws://relay.example:8080/community/?x=1"
        );
    }

    #[test]
    fn rejects_non_relay_and_ambiguous_urls() {
        assert_eq!(
            normalize_relay_url("https://relay.example").unwrap_err(),
            NormalizeRelayUrlError::InvalidScheme
        );
        assert_eq!(
            normalize_relay_url("wss://user@relay.example").unwrap_err(),
            NormalizeRelayUrlError::Credentials
        );
        assert_eq!(
            normalize_relay_url("wss://relay.example/#x").unwrap_err(),
            NormalizeRelayUrlError::Fragment
        );
    }
}
