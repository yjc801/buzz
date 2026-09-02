//! Generic outbound relay-to-webhook bridge.
//!
//! Subscribes to a Buzz relay over WebSocket with its own Nostr identity,
//! matches incoming events against configured [`rules::Rule`]s, and fires
//! templated HTTP webhooks. Deliberately not coupled to GitHub or to any
//! specific consumer — every real use is expressed purely as configuration
//! (see the crate README for the worked GitHub Actions dispatch example).
//!
//! # Loop safety: the bridge never writes to the relay
//!
//! The structural argument first: this daemon holds a read-only relationship
//! with the relay. It sends REQ frames and NIP-42 AUTH, and nothing else —
//! there is no code path that publishes an event, so the bridge cannot feed
//! its own subscriptions no matter how its rules are configured.
//!
//! The residual hazard is indirect: a rule whose webhook *causes* relay
//! events that match the same rule (webhook → CI job → CI posts a relay
//! event → webhook …). Two guards bound that loop:
//!
//! 1. **The `authors` filter.** Every rule pins the exact author pubkeys it
//!    reacts to, so a loop requires the webhook's downstream side to publish
//!    *as one of those pinned authors* — not merely to publish something.
//! 2. **The per-rule token bucket.** Even a genuine loop cannot dispatch
//!    faster than the rule's `max_per_minute` budget (default 6/min);
//!    over-budget matches are logged and dropped, so a runaway feedback
//!    cycle degrades into a bounded trickle instead of a storm.
//!
//! # Delivery semantics: a latency optimizer, not a delivery guarantee
//!
//! The bridge is **at-least-once, best-effort**. Its job is to make the
//! common case fast — an event arrives, the webhook fires within seconds —
//! not to guarantee that every matching event produces exactly one call.
//! Consumers are expected to keep their own polling fallback (the worked
//! example's GitHub workflow keeps its cron schedule); the bridge only
//! collapses the usual multi-minute polling latency.
//!
//! Concretely:
//!
//! - Each (re)subscribe asks for a 600-second overlap
//!   ([`bridge::RESUBSCRIBE_OVERLAP_SECS`]), so events that arrived while
//!   the bridge was reconnecting are replayed rather than lost.
//! - An in-memory ring of seen event ids ([`dispatch::SeenRing`], capacity
//!   4096) suppresses duplicate deliveries *within one process lifetime*.
//!   There is no durable state and no volume: a restart may re-deliver
//!   whatever the overlap window replays, which at-least-once permits.
//! - A failed delivery is retried once (transport error or 5xx), then
//!   logged and dropped. The polling fallback owns everything past that.

#![deny(unsafe_code)]

pub mod bridge;
pub mod config;
pub mod dispatch;
pub mod rules;
