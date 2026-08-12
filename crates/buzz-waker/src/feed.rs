//! The event feed — §4 of `PLANS/BUZZ_WAKER_DESIGN.md`.
//!
//! Everything before this module decides things; this is what gives it
//! something to decide about. It holds this agent's relay subscriptions —
//! one live subscription per channel it belongs to, a membership-change
//! watch, and a paged backfill walk — folds each event through the durable
//! cursor, and survives the connection dropping without losing the events
//! that arrived while it was gone.
//!
//! # Shape
//!
//! The same split as [`crate::attempt`]: the policy is pure and the socket is
//! behind [`FeedTransport`]. So the parts that are easy to get wrong — the
//! filters, the reconnect ladder, what each relay frame means — are tested
//! directly, and the transport is a thin adapter with nothing to reason about.
//!
//! # Why live fan-out is per channel, not one filter
//!
//! The relay scopes channel-event fan-out to subscriptions that carry that
//! channel's `#h` tag (`buzz-relay/src/subscription.rs::fan_out_scoped`) —
//! by design, a channel-less subscription never receives channel-scoped
//! events, the same invariant that keeps one channel's traffic from leaking
//! to a global subscriber. A single `#p`-only filter with no `#h` is
//! therefore structurally unable to receive live channel mentions at all; it
//! only ever worked for *historical* delivery, because the relay's REQ
//! handler resolves `accessible_channels` from the authenticated pubkey and
//! scopes the historical query to it without needing `#h` on the filter
//! (`buzz-relay/src/handlers/req.rs`). Unit tests that inject [`FeedFrame`]s
//! directly never exercised the registry and so never caught this — it only
//! showed up testing against a real relay.
//!
//! The fix is one live subscription per channel the agent belongs to,
//! `#h`-scoped, discovered via [`FeedTransport::discover_channels`] and kept
//! current by [`FeedTransport::subscribe_membership`]. The single paged
//! backfill walk is untouched: it was never the broken part, and the relay's
//! own access-scoping means it needs no `#h` to begin with.
//!
//! # The required connect order
//!
//! [`FeedTransport::discover_channels`], then
//! [`FeedTransport::subscribe_membership`], then
//! [`FeedTransport::subscribe_channel_live`] for every discovered channel —
//! **all before** the first [`FeedTransport::subscribe_backfill`] call. Fan-out
//! registration must be live before the historical walk starts, or a mention
//! published in the gap is neither in the page that walk fetches (it hasn't
//! happened yet) nor delivered live (nothing is subscribed yet) and is lost
//! for good. The backfill walk itself does not depend on this order — it is
//! `#h`-less and access-scoped server-side — but the live fence around it
//! does, and starting backfill first is exactly the gap this note exists to
//! prevent. Every one of these calls takes the *same* `since` — the cursor's
//! replay floor — never connect time; see "Why every live subscription's
//! `since` is the cursor floor" below for the race that requirement closes.
//!
//! An empty discovered-channel set changes nothing about this order: the
//! backfill REQ is unconditional, never gated on how many (if any) live
//! subscriptions are open. See [`WakeReplay::new`].
//!
//! # Why every live subscription's `since` is the cursor floor, not connect time
//!
//! An earlier version of this design bound each per-channel live REQ's
//! `since` to connect time — reasoning that these subscriptions exist purely
//! to catch fan-out from the moment they open, so nothing older should
//! matter to them. That is wrong, and the bug it left is a real event loss,
//! not a cosmetic one: a mention's `created_at` is client-signed, and
//! ordinary clock skew or a brief queue-then-publish delay (both within the
//! relay's accepted drift) can leave it a few seconds *behind* wall-clock
//! "now" even though it is being published live, right now. A live
//! subscription whose `since` is connect time rejects that event outright
//! (`created_at < since`), and if the backfill walk has already paged past
//! that timestamp by the time the event lands, nothing will ever fetch it —
//! backfill pages strictly backward and never revisits a window once it has
//! moved past it. The event is gone for good, silently.
//!
//! The fix: every subscribe call that can carry a `since` —
//! [`FeedTransport::subscribe_channel_live`] and
//! [`FeedTransport::subscribe_membership`], same as
//! [`FeedTransport::subscribe_backfill`] — uses the **same** value, the one
//! [`CursorStore::resume`] produces (already overlap-adjusted for exactly
//! this class of skew, see [`crate::RECONNECT_OVERLAP_SECS`]) and that
//! [`WakeReplay::since`] then holds for the life of the connection attempt.
//! That floor sits far enough in the past that ordinary drift can never push
//! a live event's `created_at` behind it, which is what actually closes the
//! race — using "now" was the bug, not the idea of a live-only subscription.
//!
//! This does mean a per-channel live REQ can carry a real historical
//! component again — up to one clamped page, same as the old single filter
//! did — but that no longer needs a state machine to reason about: its EOSE
//! still means nothing to [`WakeReplay`] (only the backfill subscription's
//! EOSE gates the walk), and any of that page's rows the backfill walk also
//! covers are exactly the ordinary duplicate case below. The one difference
//! from connect time is by design: a first, real historical page per
//! channel, redundant with backfill, instead of a guaranteed-empty one.
//!
//! # Duplicate delivery is normal
//!
//! A mention published while backfill is walking can arrive twice — once
//! live, once from backfill's own paging, since both windows can now
//! overlap all the way back to the shared floor — and [`CursorStore::admit`]
//! already collapses that. Nothing here tries to prevent the overlap; it is
//! cheaper to dedupe than to coordinate.
//!
//! # Three more things that are not obvious
//!
//! - **Every filter must carry `kinds`.** A filter without them trips the
//!   relay's p-gate and comes back 403 (`CLAUDE.md` § Common Gotchas), which
//!   reads as a permission problem rather than a malformed query.
//!   [`WAKE_TRIGGER_KINDS`] is the right set for a mention filter anyway:
//!   [`crate::decide`] discards everything else, so asking for less traffic
//!   costs nothing.
//! - **One feed per agent, bound to that agent's own connection.** The relay
//!   scopes a REQ's history to the channels the *authenticated* pubkey can
//!   reach and re-checks that same connection's membership on live
//!   private-channel fan-out — see [`wake_filter`]. A watch list folded into
//!   one connection would silently drop another agent's private-channel
//!   mentions, so [`FeedTransport`] takes one pubkey, not a list.
//! - **The waker connects interactive**, sending no `class` tag. The
//!   read-only connection class exists in this repo's relay but is not
//!   deployed on the relay we use, and the client fails closed when a
//!   requested class is not confirmed — so requesting it would refuse every
//!   connection. §4's deployment amendment records the consequence: a
//!   cleanly-exiting watched agent's presence dot lingers for up to its 180s
//!   TTL instead of clearing at once. Bounded, and the bound holds because a
//!   watcher never publishes presence and so never refreshes that TTL.

use serde_json::{json, Value};
use uuid::Uuid;

use crate::cursor::{Admission, CursorStore};
use crate::decide::{normalize_pubkey, TriggerEvent, WAKE_TRIGGER_KINDS};

/// Subscription id for the **backfill** walk: `until`-bounded after its first
/// page, re-issued once per page, and closed when the backlog is drained.
///
/// One per connection, never per channel — see the module docs on why the
/// relay's own access scoping makes that unnecessary.
pub const WAKE_BACKFILL_SUBSCRIPTION_ID: &str = "buzz-waker-backfill";

/// Subscription id for the membership-change watch: unbounded, opened once
/// per connection, notifying this agent when it is added to or removed from
/// a channel so [`FeedStep::ChannelMembershipChanged`] can keep the set of
/// open live subscriptions current.
pub const WAKE_MEMBERSHIP_SUBSCRIPTION_ID: &str = "buzz-waker-membership";

/// Prefix for a per-channel live subscription id — see
/// [`wake_live_subscription_id`].
const WAKE_LIVE_SUBSCRIPTION_PREFIX: &str = "buzz-waker-live-";

/// The relay subscription id for one channel's live feed.
///
/// Deterministic from the channel id rather than random, so a reconnect (or a
/// re-subscribe after a membership-added notification for a channel already
/// tracked) replaces the existing subscription instead of accumulating a
/// second one, and so relay-side logs stay legible across restarts.
#[must_use]
pub fn wake_live_subscription_id(channel_id: Uuid) -> String {
    format!("{WAKE_LIVE_SUBSCRIPTION_PREFIX}{channel_id}")
}

/// Which of the feed's subscriptions a frame arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeSubscription {
    /// A per-channel live subscription — fan-out only, no historical
    /// component. See the module docs on why.
    Live(Uuid),
    /// The paged walk over the stored backlog. One per connection, global —
    /// never `#h`-scoped.
    Backfill,
    /// The membership-change watch for this agent's own channel list.
    Membership,
}

impl WakeSubscription {
    /// Resolve a relay subscription id, or `None` if it is not one of ours.
    #[must_use]
    pub fn from_id(subscription_id: &str) -> Option<Self> {
        match subscription_id {
            WAKE_BACKFILL_SUBSCRIPTION_ID => return Some(Self::Backfill),
            WAKE_MEMBERSHIP_SUBSCRIPTION_ID => return Some(Self::Membership),
            _ => {}
        }
        subscription_id
            .strip_prefix(WAKE_LIVE_SUBSCRIPTION_PREFIX)
            .and_then(|rest| rest.parse::<Uuid>().ok())
            .map(Self::Live)
    }
}

/// How long to wait for a frame before treating the connection as idle.
///
/// Idle is not failure — a quiet channel is the normal case. It is the cue to
/// checkpoint coverage ([`CursorStore::mark_covered`]) so a later restart can
/// tell "nothing happened" from "we were not watching".
pub const FEED_IDLE_TIMEOUT_SECS: u64 = 60;

/// First rung of the reconnect ladder.
pub const RECONNECT_BASE_DELAY_MS: u64 = 500;

/// Ceiling on the reconnect ladder.
///
/// A waker that has been failing for an hour should still retry every 30s: the
/// cost of a retry is one WebSocket handshake, and the cost of backing off
/// further is a mention nobody answers.
pub const RECONNECT_MAX_DELAY_MS: u64 = 30_000;

/// Shortest `retry in {N}s` hint a relay is taken at its word for.
///
/// Below this the hint is treated as "no useful number" and replaced by
/// [`RATE_LIMIT_FLOOR_SECS`]. `buzz-acp` draws the same line for the same
/// relay behaviour.
pub const RATE_LIMIT_MIN_HINT_SECS: u64 = 2;

/// Park duration substituted for a missing or too-short rate-limit hint.
///
/// The relay this daemon runs against answers a rate-limited subscription
/// with `retry in 0s`. Taking that literally is precisely what turns one
/// close into an unbounded hot loop: the retry re-trips the quota, the relay
/// closes again, and — because the send itself succeeded — nothing on the
/// reconnect ladder ever counts it. This floor is what ends that loop.
pub const RATE_LIMIT_FLOOR_SECS: u64 = 5;

/// Whether a membership-change notification added or removed this agent from
/// a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipChangeKind {
    /// The agent was added to the channel — open a live subscription for it.
    Added,
    /// The agent was removed from the channel — close its live subscription.
    Removed,
}

/// What a relay frame means to the feed.
///
/// Deliberately smaller than the relay's message set: the feed reacts to a
/// handful of things and ignores the rest, and naming the ignored ones here
/// rather than matching them at the call site keeps the loop honest about
/// what it drops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedSignal {
    /// An event on one of our subscriptions that parsed into a candidate
    /// trigger.
    Trigger {
        /// Which subscription delivered it — the tally that drives paging is
        /// per subscription, because a live subscription keeps delivering
        /// long after the backfill walk it runs alongside has finished.
        sub: WakeSubscription,
        /// The candidate.
        event: Box<TriggerEvent>,
    },
    /// An event on one of our subscriptions the feed could not read. Distinct
    /// from [`FeedSignal::Ignored`] because it still consumed a row of the
    /// relay's page, which is what [`WakeReplay`] counts — but it offers no
    /// `created_at`, so it cannot be paged on.
    Unparsed {
        /// Which subscription delivered it.
        sub: WakeSubscription,
    },
    /// An event whose id or signature did not verify. The relay either forged
    /// it or served something corrupted; either way none of its fields may be
    /// believed, including its timestamp.
    Rejected {
        /// Which subscription delivered it.
        sub: WakeSubscription,
        /// The id the event claimed. Only useful for correlating relay logs —
        /// on an id mismatch it is by definition not the event's real id.
        event_id: String,
        /// Why verification failed.
        reason: String,
    },
    /// Stored events for that subscription's current page are drained.
    ReplayComplete {
        /// Which subscription finished a page.
        sub: WakeSubscription,
    },
    /// The relay closed one of our subscriptions. What that means depends on
    /// which one — see [`step`].
    Closed {
        /// Which subscription was closed.
        sub: WakeSubscription,
        /// The relay's stated reason. Empty when it is merely acknowledging a
        /// CLOSE we sent.
        reason: String,
    },
    /// This agent was added to or removed from a channel.
    MembershipChanged {
        /// The channel that changed.
        channel_id: Uuid,
        /// Added or removed.
        change: MembershipChangeKind,
    },
    /// Nothing the feed acts on.
    Ignored,
}

/// A relay frame, reduced to what the feed reads.
///
/// The feed defines its own frame type rather than matching on the ws client's
/// so that [`classify`] — the part with the subscription-id and parsing rules
/// worth testing — needs no socket, no runtime, and no relay to exercise.
#[derive(Debug, Clone)]
pub enum FeedFrame {
    /// An event delivered on some subscription.
    Event {
        /// The subscription the relay attributed it to.
        subscription_id: String,
        /// The raw event JSON.
        event: Value,
    },
    /// End of stored events for a subscription.
    Eose {
        /// The subscription that finished its replay.
        subscription_id: String,
    },
    /// The relay closed a subscription.
    Closed {
        /// The subscription that was closed.
        subscription_id: String,
        /// The relay's stated reason.
        message: String,
    },
    /// An EVENT the transport refused to project because its id or signature
    /// did not verify — see [`crate::relay_feed`], which is the only place a
    /// signature still exists to check.
    Rejected {
        /// The subscription the relay attributed it to.
        subscription_id: String,
        /// The id the event claimed.
        event_id: String,
        /// Why verification failed.
        reason: String,
    },
    /// Anything else — NOTICE, OK, COUNT, a late AUTH challenge.
    Other,
}

/// The REQ filter for one agent's mention feed, optionally bounded to a page
/// and/or scoped to one channel.
///
/// `#p` is the addressing the harness itself keys on, so it is what the feed
/// asks for; `kinds` is mandatory (see the module note); `since` is
/// [`CursorStore::resume`]'s output for every subscription this builds a
/// filter for — backfill, a per-channel live subscription, or the membership
/// watch all share the one floor. See the module docs on why a per-channel
/// live subscription must not use connect time instead.
///
/// # One agent per filter, because one agent per connection
///
/// This takes a single pubkey rather than a watch list, and the difference is
/// not cosmetic. The relay scopes a REQ's history to the channels the
/// *authenticated* pubkey can reach (`req.rs:88-107`) and re-checks that same
/// connection's membership on live private-channel fan-out (`event.rs:99+`).
/// So a filter naming agents A and B, sent over a connection authenticated as
/// A, returns A's mentions and **silently drops B's private-channel ones** —
/// no error, no CLOSED, just a feed that looks healthy and misses the wakes
/// that matter most.
///
/// [`FeedTransport`]'s subscribe methods are the only paths that reach a relay,
/// and they build this from the identity they authenticated with rather than
/// from an argument, so the filter and the connection cannot disagree.
///
/// `until` bounds a backfill page — `None` is either the live subscription (no
/// bound at all) or the backfill walk's first, unbounded page. `channel_id`
/// adds `#h`, turning this into a per-channel live filter; `None` is the
/// (global) backfill shape.
#[must_use]
pub fn wake_filter(
    agent_pubkey: &str,
    since: u64,
    until: Option<u64>,
    channel_id: Option<Uuid>,
) -> Value {
    let mut filter = json!({
        "kinds": WAKE_TRIGGER_KINDS,
        "#p": [normalize_pubkey(agent_pubkey)],
        "since": since,
        "limit": REPLAY_PAGE_LIMIT,
    });
    if let Some(object) = filter.as_object_mut() {
        if let Some(until) = until {
            object.insert("until".to_string(), json!(until));
        }
        if let Some(channel_id) = channel_id {
            object.insert("#h".to_string(), json!([channel_id.to_string()]));
        }
    }
    filter
}

/// The REQ frame opening one channel's live subscription.
///
/// `since` must be the cursor's replay floor ([`CursorStore::resume`]'s
/// output) — the same value passed to [`wake_backfill_req`] and
/// [`wake_membership_req`] for this connection attempt, never connect time.
/// See the module docs for the race a connect-time `since` opens.
#[must_use]
pub fn wake_live_req(agent_pubkey: &str, channel_id: Uuid, since: u64) -> Value {
    json!([
        "REQ",
        wake_live_subscription_id(channel_id),
        wake_filter(agent_pubkey, since, None, Some(channel_id))
    ])
}

/// The CLOSE frame retiring one channel's live subscription, e.g. after a
/// membership-removed notification.
#[must_use]
pub fn wake_live_close(channel_id: Uuid) -> Value {
    json!(["CLOSE", wake_live_subscription_id(channel_id)])
}

/// The REQ frame for one page of the **backfill** walk.
///
/// `until = None` is the first page — unbounded, matching the shape the old
/// single live subscription used for its own historical component. Every
/// page after the first carries the bound [`WakeReplay::on_eose`] computed
/// from the previous one.
#[must_use]
pub fn wake_backfill_req(agent_pubkey: &str, since: u64, until: Option<u64>) -> Value {
    json!([
        "REQ",
        WAKE_BACKFILL_SUBSCRIPTION_ID,
        wake_filter(agent_pubkey, since, until, None)
    ])
}

/// The CLOSE frame retiring the backfill subscription once the walk is done.
#[must_use]
pub fn wake_backfill_close() -> Value {
    json!(["CLOSE", WAKE_BACKFILL_SUBSCRIPTION_ID])
}

/// The REQ filter for the membership-change watch: kind:44100/44101
/// (member added/removed notification), `#p` = this agent.
#[must_use]
pub fn wake_membership_filter(agent_pubkey: &str, since: u64) -> Value {
    json!({
        "kinds": [
            buzz_core::kind::KIND_MEMBER_ADDED_NOTIFICATION,
            buzz_core::kind::KIND_MEMBER_REMOVED_NOTIFICATION,
        ],
        "#p": [normalize_pubkey(agent_pubkey)],
        "since": since,
    })
}

/// The REQ frame opening the membership-change watch.
#[must_use]
pub fn wake_membership_req(agent_pubkey: &str, since: u64) -> Value {
    json!([
        "REQ",
        WAKE_MEMBERSHIP_SUBSCRIPTION_ID,
        wake_membership_filter(agent_pubkey, since)
    ])
}

/// Rows to ask for per replay page.
///
/// Sent explicitly rather than left to the relay's default because the whole
/// paging protocol turns on one question — *did the relay clamp this page, or
/// did it run out of events?* — and NIP-01 has no flag for it. A page that
/// comes back with exactly the number of rows we asked for is the only
/// available evidence that there is more behind it.
///
/// Chosen below the relay's own ceiling (`DEFAULT_MAX_PAGE_LIMIT` = 1000 in
/// `buzz-db`) rather than equal to it, so the value we compare against is one
/// we set. Not pinned to that constant by a contract test on purpose: the
/// deployment amendment in §4 settles that **we do not host the relay we talk
/// to**, so its page cap is not this repo's constant and asserting equality
/// would be false precision.
///
/// # The bound this leaves
///
/// A relay that silently applies a cap *smaller* than this would return short
/// pages forever and replay would stop early believing it had drained. That is
/// the same failure the single-REQ version had unconditionally, now needing a
/// relay with a sub-500 cap to trigger, and it is visible: §5's runtime
/// coverage-age alert is what catches it.
pub const REPLAY_PAGE_LIMIT: u32 = 500;

/// Ceiling on REQs issued for one connection's replay.
///
/// At [`REPLAY_PAGE_LIMIT`] rows a page this is 32,000 events, which is far
/// past any real outage backlog for one agent's mentions. It exists so a relay
/// that keeps answering "here is a full page" can never hold the feed in
/// replay indefinitely — a waker stuck draining history is a waker answering
/// nobody, which is worse than a truncated backfill it reports.
pub const MAX_REPLAY_PAGES: u32 = 64;

/// What one page of replay contained.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PageTally {
    /// Rows the relay served on our subscription, whether or not the feed
    /// could read them. This is what the relay's clamp counts.
    count: u32,
    /// Oldest `created_at` in the page, and so the bound for the next one.
    /// `None` when no row in the page could be parsed.
    oldest: Option<u64>,
}

/// What the caller should do once a page's EOSE arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayStep {
    /// Issue the backfill REQ with these bounds. The backlog is **not**
    /// drained, and the cursor's replay pin must stay held.
    Backfill {
        /// Unchanged for the whole replay — the floor from
        /// [`CursorStore::resume`].
        since: u64,
        /// The inclusive upper bound for this page.
        until: u64,
    },
    /// The stored backlog is drained. Close the backfill subscription; every
    /// live subscription stays up.
    Complete {
        /// The walk stopped before reaching `since`. Everything older than the
        /// last page it managed to read was never delivered and never will be:
        /// re-issuing the same request returns the same rows.
        truncated: bool,
    },
}

/// Drains the stored backlog one page at a time, **on the connection's one
/// backfill subscription**, entirely decoupled from how many live
/// subscriptions are open.
///
/// # Why EOSE is not the end of history
///
/// The relay clamps every historical filter to a page
/// (`buzz-db::DEFAULT_MAX_PAGE_LIMIT`), orders it `created_at DESC, id ASC`,
/// and then sends EOSE. So after an outage that accumulated more than one page
/// of mentions, one `since` REQ delivers the *newest* page and EOSE says
/// "stored events drained" about that page, not about history. Treating it as
/// the latter releases the cursor's replay pin over events that were never
/// delivered, and because the request is deterministic, repeating it returns
/// the same newest page forever — older mentions skipped permanently and
/// silently.
///
/// # Why this never looks at a live subscription's EOSE
///
/// An earlier design started the walk only once a single live subscription's
/// own historical page came back clamped, treating that as the evidence
/// there was history behind it. Per-channel live subscriptions make that
/// signal expensive to reconstruct correctly — N channels each reporting
/// independently, needing to wait for all of them before it is safe to
/// decide — for no benefit, since the backfill walk can simply always run:
/// it is one extra, usually-short REQ per connection when there is nothing
/// behind, against real state-machine complexity when there is. See
/// [`WakeReplay::new`], which always starts already walking.
///
/// Rows delivered on a live subscription are never tallied here — see
/// [`WakeSubscription::Live`] — even though a live subscription now shares
/// the backfill floor and so can carry a real historical page of its own
/// (see the module docs on why connect time was the wrong bound). Duplicate
/// deliveries between backfill's paging and a live subscription's own
/// floor-onward page are the normal case, and [`CursorStore::admit`] already
/// collapses them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeReplay {
    since: u64,
    phase: ReplayPhase,
    page: PageTally,
    pages_issued: u32,
}

/// Which page is draining, and so what the next EOSE means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplayPhase {
    /// The backfill subscription is walking down. `until` is `None` for the
    /// first, unbounded page and `Some` for every page after.
    Backfill {
        /// The inclusive bound the standing page carries, if any.
        until: Option<u64>,
    },
    /// The backlog is drained. Only live traffic remains.
    Done,
}

impl WakeReplay {
    /// Start a replay at `since`, which the caller has already taken from
    /// [`CursorStore::resume`].
    ///
    /// One per connection attempt: `since` is re-derived on reconnect, and the
    /// page budget should be spent per connection rather than for the lifetime
    /// of the process. Starts **already walking** — the caller's first
    /// backfill REQ (unbounded, `until = None`) is issued unconditionally, not
    /// gated on any live subscription's own history, and not gated on how
    /// many channels (if any) were discovered. An agent in zero channels still
    /// gets a backfill walk; it will simply come back empty.
    #[must_use]
    pub fn new(since: u64) -> Self {
        Self {
            since,
            phase: ReplayPhase::Backfill { until: None },
            page: PageTally::default(),
            // The caller's first backfill subscribe is page one.
            pages_issued: 1,
        }
    }

    /// The floor every page of this replay asks from.
    #[must_use]
    pub fn since(&self) -> u64 {
        self.since
    }

    /// The bound the standing backfill page carries. `None` for the first
    /// (unbounded) page, and also once the walk is done — callers only read
    /// this while [`WakeReplay::is_complete`] is `false`.
    #[must_use]
    pub fn backfill_until(&self) -> Option<u64> {
        match self.phase {
            ReplayPhase::Backfill { until } => until,
            ReplayPhase::Done => None,
        }
    }

    /// Whether the stored backlog is drained and only live traffic remains.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(self.phase, ReplayPhase::Done)
    }

    /// Count one row the relay served, if it belongs to the backfill page
    /// currently draining.
    ///
    /// Rows a live subscription delivers are never tallied — they carry no
    /// historical component and are not a page to page on. `created_at` is
    /// `None` for a row the feed could not read or would not believe: it
    /// still consumed a row of the relay's page, so it counts, but it
    /// contributes no bound to page on.
    fn observe_row(&mut self, sub: WakeSubscription, created_at: Option<u64>) {
        if !matches!(
            (self.phase, sub),
            (ReplayPhase::Backfill { .. }, WakeSubscription::Backfill)
        ) {
            return;
        }

        self.page.count = self.page.count.saturating_add(1);
        if let Some(created_at) = created_at {
            self.page.oldest = Some(match self.page.oldest {
                Some(oldest) => oldest.min(created_at),
                None => created_at,
            });
        }
    }

    /// Fold a backfill EOSE and say whether the backlog is actually drained.
    ///
    /// Returns `None` for any EOSE that is not the backfill subscription's —
    /// a live subscription's EOSE means nothing here (see the module docs) —
    /// or that arrives after the walk has already finished.
    pub fn on_eose(&mut self, sub: WakeSubscription) -> Option<ReplayStep> {
        if !matches!(sub, WakeSubscription::Backfill) || matches!(self.phase, ReplayPhase::Done) {
            return None;
        }

        let page = std::mem::take(&mut self.page);
        // The only evidence available that the relay clamped rather than ran
        // out of rows — NIP-01 has no "there is more" flag.
        let clamped = page.count >= REPLAY_PAGE_LIMIT;

        if !clamped {
            // The relay ran out of rows before it ran out of page: nothing
            // older is behind this one.
            return Some(self.finish(false));
        }

        let standing_bound = self.backfill_until();
        match page.oldest {
            // `until` is inclusive, so a full page whose oldest row sits on the
            // bound we already asked for cannot advance — more than a page of
            // events share that one second, and asking again returns them
            // again. Refusing to loop is the only correct move.
            Some(oldest)
                if standing_bound != Some(oldest) && self.pages_issued < MAX_REPLAY_PAGES =>
            {
                self.phase = ReplayPhase::Backfill {
                    until: Some(oldest),
                };
                self.pages_issued = self.pages_issued.saturating_add(1);
                Some(ReplayStep::Backfill {
                    since: self.since,
                    until: oldest,
                })
            }
            // Out of page budget, stalled on a tied second, or a full page in
            // which nothing parsed and so left no bound to page on.
            _ => Some(self.finish(true)),
        }
    }

    /// The backfill subscription went away before the walk finished.
    ///
    /// Not a reconnect: every live subscription is untouched and still the
    /// one that matters, so the feed keeps working and the backlog is simply
    /// reported as cut short. Returns `None` when no walk was in progress —
    /// which is the ordinary case, because the relay answers the CLOSE the
    /// caller sends on completion with a CLOSED of its own
    /// (`handlers/close.rs`), and reading that echo as a failure would put the
    /// feed into a reconnect loop on every clean replay.
    pub fn on_backfill_closed(&mut self) -> Option<ReplayStep> {
        match self.phase {
            ReplayPhase::Backfill { .. } => Some(self.finish(true)),
            ReplayPhase::Done => None,
        }
    }

    fn finish(&mut self, truncated: bool) -> ReplayStep {
        self.phase = ReplayPhase::Done;
        ReplayStep::Complete { truncated }
    }
}

/// Read a relay event into the fields a wake decision needs.
///
/// Returns `None` for anything structurally unusable — a missing id, author or
/// `created_at`, or a kind outside `u32`. Refusing beats defaulting: every
/// field here feeds a decision that spends a real deploy, and an event with an
/// invented author would be evaluated against the wrong respond-to policy.
///
/// Only `p` tags are read. `h`, `e` and the rest are deliberately dropped —
/// the decision does not use them, and not copying them keeps the feed's
/// in-memory footprint to the fields §4's R4 names.
#[must_use]
pub fn trigger_event_from_json(event: &Value) -> Option<Box<TriggerEvent>> {
    let id = event.get("id")?.as_str()?.trim();
    let author = event.get("pubkey")?.as_str()?.trim();
    if id.is_empty() || author.is_empty() {
        return None;
    }
    let kind = u32::try_from(event.get("kind")?.as_u64()?).ok()?;
    let created_at = event.get("created_at")?.as_u64()?;

    let p_tags = event
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(|tag| {
                    let parts = tag.as_array()?;
                    if parts.first()?.as_str()? != "p" {
                        return None;
                    }
                    Some(parts.get(1)?.as_str()?.to_string())
                })
                .collect()
        })
        .unwrap_or_default();

    Some(Box::new(TriggerEvent {
        id: id.to_string(),
        author: author.to_string(),
        kind,
        p_tags,
        created_at,
    }))
}

/// Read a membership-change notification event into the channel it names and
/// whether it is an add or a remove.
///
/// Returns `None` for a kind that is not one of the two membership-change
/// kinds, or an event with no `h` tag naming a channel — either is a
/// malformed frame on our own subscription (see [`WAKE_MEMBERSHIP_SUBSCRIPTION_ID`]),
/// not a value to guess at.
#[must_use]
fn membership_change_from_json(event: &Value) -> Option<(Uuid, MembershipChangeKind)> {
    let kind = u32::try_from(event.get("kind")?.as_u64()?).ok()?;
    let change = if kind == buzz_core::kind::KIND_MEMBER_ADDED_NOTIFICATION {
        MembershipChangeKind::Added
    } else if kind == buzz_core::kind::KIND_MEMBER_REMOVED_NOTIFICATION {
        MembershipChangeKind::Removed
    } else {
        return None;
    };

    let channel_id = event.get("tags")?.as_array()?.iter().find_map(|tag| {
        let parts = tag.as_array()?;
        if parts.first()?.as_str()? != "h" {
            return None;
        }
        parts.get(1)?.as_str()?.parse::<Uuid>().ok()
    })?;

    Some((channel_id, change))
}

/// What a frame means to the feed.
///
/// Frames for another subscription id are [`FeedSignal::Ignored`], not an
/// error: one connection legitimately carries several subscriptions, and
/// silently acting on another's events would wake agents from a filter this
/// module never wrote.
#[must_use]
pub fn classify(frame: &FeedFrame) -> FeedSignal {
    let subscription_id = match frame {
        FeedFrame::Event {
            subscription_id, ..
        }
        | FeedFrame::Eose { subscription_id }
        | FeedFrame::Closed {
            subscription_id, ..
        }
        | FeedFrame::Rejected {
            subscription_id, ..
        } => subscription_id,
        FeedFrame::Other => return FeedSignal::Ignored,
    };
    let Some(sub) = WakeSubscription::from_id(subscription_id) else {
        return FeedSignal::Ignored;
    };

    match frame {
        FeedFrame::Event { event, .. } if sub == WakeSubscription::Membership => {
            match membership_change_from_json(event) {
                Some((channel_id, change)) => FeedSignal::MembershipChanged { channel_id, change },
                None => FeedSignal::Unparsed { sub },
            }
        }
        FeedFrame::Event { event, .. } => {
            trigger_event_from_json(event).map_or(FeedSignal::Unparsed { sub }, |event| {
                FeedSignal::Trigger { sub, event }
            })
        }
        FeedFrame::Eose { .. } => FeedSignal::ReplayComplete { sub },
        FeedFrame::Closed { message, .. } => FeedSignal::Closed {
            sub,
            reason: message.clone(),
        },
        FeedFrame::Rejected {
            event_id, reason, ..
        } => FeedSignal::Rejected {
            sub,
            event_id: event_id.clone(),
            reason: reason.clone(),
        },
        FeedFrame::Other => FeedSignal::Ignored,
    }
}

/// Delay before the next connection attempt, in milliseconds.
///
/// Doubling from [`RECONNECT_BASE_DELAY_MS`], capped at
/// [`RECONNECT_MAX_DELAY_MS`]. `consecutive_failures` is the count *before*
/// this attempt, so the first retry after one failure waits the base delay.
///
/// No jitter, deliberately: jitter exists to de-synchronise a fleet, and there
/// is one waker per owner. Adding it here would only make the ladder
/// untestable.
#[must_use]
pub fn reconnect_delay_ms(consecutive_failures: u32) -> u64 {
    if consecutive_failures == 0 {
        return 0;
    }
    RECONNECT_BASE_DELAY_MS
        .saturating_mul(1_u64 << consecutive_failures.saturating_sub(1).min(16))
        .min(RECONNECT_MAX_DELAY_MS)
}

/// Whether a relay's close `reason` says the subscription was rate limited.
///
/// Keys off NIP-01's machine-readable `rate-limited:` prefix rather than the
/// free-form prose after it, which is relay-specific. `buzz-acp` matches the
/// same prefix for the same reason.
#[must_use]
pub fn is_rate_limited(reason: &str) -> bool {
    reason.starts_with("rate-limited:")
}

/// Parse a relay's `retry in {N}s` hint out of a close reason.
///
/// Accepts any string containing `retry in ` followed by decimal digits.
/// Returns `None` when there is no hint, and `Some(0)` for a literal zero —
/// flooring is [`rate_limit_park_ms`]'s job, so that a caller cannot get a
/// zero delay by reading this value directly. A split is enough; no regex.
#[must_use]
pub fn parse_rate_limit_retry_secs(reason: &str) -> Option<u64> {
    let after = reason.split("retry in ").nth(1)?;
    // Hint digits are ASCII, so the char count equals the byte count and the
    // subslice below lands on a character boundary.
    let len = after.chars().take_while(|c| c.is_ascii_digit()).count();
    after[..len].parse::<u64>().ok()
}

/// How long to park a channel whose live subscription the relay rate limited.
///
/// `hint_secs` is the relay's own value when it offered one
/// ([`parse_rate_limit_retry_secs`]). `consecutive` counts this channel's
/// rate-limited closes on the current connection *before* this one, so the
/// first park is the unmultiplied floor.
///
/// An ordinary close is independent, and repairing it immediately is correct.
/// A rate-limited close is neither independent nor transient: it trips every
/// channel at once, and the retry is itself what re-trips it. So a missing or
/// sub-[`RATE_LIMIT_MIN_HINT_SECS`] hint is synthesized from
/// [`RATE_LIMIT_FLOOR_SECS`], doubled per consecutive close and capped at
/// [`RECONNECT_MAX_DELAY_MS`] — the same shape and ceiling as
/// [`reconnect_delay_ms`]. A *valid* hint is the relay's own read of its
/// limiter and is honoured as a floor instead: the local ceiling only bounds
/// the synthesized fallback, never a real hint, so a `retry in 60s` is never
/// truncated into a retry that arrives before the relay's own reset and
/// re-trips the same limiter.
///
/// Deterministic and jitter-free, for the reason given on
/// [`reconnect_delay_ms`]. The channels that all park together are spread by
/// the caller's staggered drain, not by randomness here.
#[must_use]
pub fn rate_limit_park_ms(hint_secs: Option<u64>, consecutive: u32) -> u64 {
    let multiplier = 1_u64 << consecutive.min(16);
    match hint_secs {
        // A valid hint only ever grows from here (multiplier >= 1), so it
        // can never need capping up to the ceiling — only down past it,
        // which is exactly the bug: never floor a real hint below itself.
        Some(secs) if secs >= RATE_LIMIT_MIN_HINT_SECS => {
            secs.saturating_mul(1_000).saturating_mul(multiplier)
        }
        _ => RATE_LIMIT_FLOOR_SECS
            .saturating_mul(1_000)
            .saturating_mul(multiplier)
            .min(RECONNECT_MAX_DELAY_MS),
    }
}

/// The socket, behind a trait so the loop above it can be tested.
///
/// One connection, owned for the transport's lifetime. `connect` is separate
/// from construction because a reconnect must be able to rebuild the socket
/// without rebuilding the caller's state — the cursor and the in-flight set
/// have to outlive any one connection.
///
/// # Required call order
///
/// [`discover_channels`](Self::discover_channels), then
/// [`subscribe_membership`](Self::subscribe_membership), then
/// [`subscribe_channel_live`](Self::subscribe_channel_live) for every
/// discovered channel — **all before** the first
/// [`subscribe_backfill`](Self::subscribe_backfill) call. See the module docs
/// for why: fan-out has to be live before the historical walk starts, or a
/// mention published in the gap is lost. Nothing in this trait's types
/// enforces the order; it is a protocol invariant the caller must honour.
pub trait FeedTransport {
    /// Errors this transport can raise.
    type Error;

    /// The pubkey this transport authenticates as.
    ///
    /// Exposed so callers can key per-agent state — the cursor, the enrolment
    /// record — off the identity the socket actually holds rather than off a
    /// list they carry separately.
    fn agent_pubkey(&self) -> &str;

    /// Establish (or re-establish) the connection and authenticate.
    fn connect(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>>;

    /// Discover the channels this agent currently belongs to.
    ///
    /// Called once per connection, before any subscribe call — see the
    /// required call order above. An empty result is not an error: an agent
    /// in zero channels still runs a backfill walk, it simply has nothing to
    /// live-subscribe to.
    fn discover_channels(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Vec<Uuid>, Self::Error>>;

    /// Open the membership-change watch under
    /// [`WAKE_MEMBERSHIP_SUBSCRIPTION_ID`]. Issue this once per connection,
    /// after [`discover_channels`](Self::discover_channels) and before any
    /// [`subscribe_channel_live`](Self::subscribe_channel_live) call, so a
    /// membership change racing with startup is never missed. `since` is the
    /// cursor's replay floor — same value, same reason, as
    /// [`subscribe_channel_live`](Self::subscribe_channel_live) below.
    fn subscribe_membership(
        &mut self,
        since: u64,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>>;

    /// Open one channel's live subscription under
    /// [`wake_live_subscription_id`]. `since` **must** be the cursor's replay
    /// floor ([`CursorStore::resume`]'s output) — the same value passed to
    /// [`subscribe_backfill`](Self::subscribe_backfill) for this connection
    /// attempt, never connect time. See the module docs for the race a
    /// connect-time `since` opens: an event whose `created_at` lands a few
    /// seconds behind wall-clock now (ordinary skew or publish delay) is
    /// silently unrecoverable once backfill has paged past it.
    ///
    /// Idempotent: re-issuing for a channel already open replaces its
    /// subscription rather than adding a second one (same relay semantics as
    /// any REQ under a repeated id), which is what makes it safe to call
    /// again on a redelivered membership-added notification.
    fn subscribe_channel_live(
        &mut self,
        channel_id: Uuid,
        since: u64,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>>;

    /// Close one channel's live subscription, e.g. after a
    /// membership-removed notification.
    fn unsubscribe_channel_live(
        &mut self,
        channel_id: Uuid,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>>;

    /// Issue one page of the backfill walk under
    /// [`WAKE_BACKFILL_SUBSCRIPTION_ID`]. `until = None` is the first,
    /// unbounded page.
    ///
    /// Re-issuing replaces the previous page rather than adding a
    /// subscription.
    fn subscribe_backfill(
        &mut self,
        since: u64,
        until: Option<u64>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>>;

    /// Retire the backfill subscription once the walk is done.
    ///
    /// Idempotent by design: the caller sends this on every
    /// [`FeedStep::ReplayComplete`], including the common case where the
    /// first page was short and no further paging was ever needed.
    fn close_backfill(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>>;

    /// Next frame, or `None` if `timeout_secs` passed with the connection
    /// still healthy. A dropped connection is an `Err`, not `None`: the
    /// difference decides between checkpointing coverage and reconnecting.
    ///
    /// The timeout is wall-clock across the whole call. A relay heartbeat is
    /// not a frame the caller asked for, and must not postpone the idle cue
    /// this returns — that cue is the only thing that advances coverage on a
    /// quiet connection.
    fn next_frame(
        &mut self,
        timeout_secs: u64,
    ) -> impl std::future::Future<Output = Result<Option<FeedFrame>, Self::Error>>;
}

/// What the feed did with one frame, for the caller to act on.
///
/// The feed does not itself wake anything: [`crate::attempt::run_wake_attempt`]
/// needs effects this module has no business holding. It hands over admitted
/// triggers and lets the daemon own the deploy.
#[derive(Debug)]
pub enum FeedStep {
    /// A fresh trigger, already claimed in the cursor. The caller must reach a
    /// terminal outcome and then call [`CursorStore::complete`] — or
    /// [`CursorStore::abandon`] — or the checkpoint stays pinned to it.
    ///
    /// **Including when the decision is to do nothing.** The feed admits every
    /// event the relay delivers on our filter, and most of them will wake
    /// nobody: `select_wake_candidates` refuses agent-authored events and
    /// authors outside an agent's respond-to policy. "Refused" is a terminal
    /// outcome like any other, and an event left in flight because it was
    /// uninteresting pins the checkpoint exactly as hard as one left in flight
    /// because it crashed.
    Admitted {
        /// The event to decide on.
        event: Box<TriggerEvent>,
        /// Set when the event was already older than a woken harness's replay
        /// window at admission. Process it anyway, but report the attempt as a
        /// failure rather than a healthy wake — see
        /// [`Admission::FreshButUndeliverable`].
        undeliverable: bool,
    },
    /// A duplicate, or a frame the feed ignores. Nothing to do.
    Nothing,
    /// The relay served an event whose id or signature did not verify. Nothing
    /// was admitted and nothing will be woken; surfaced rather than dropped
    /// because a relay serving forged events is an incident the daemon should
    /// report, not a quiet no-op.
    Rejected {
        /// The id the event claimed.
        event_id: String,
        /// Why verification failed.
        reason: String,
    },
    /// The stored backlog has another page behind it. Call
    /// [`FeedTransport::subscribe_backfill`] with these bounds; the replay is
    /// **not** complete and the cursor's replay pin stays held. Every live
    /// subscription is untouched and keeps delivering throughout.
    Backfill {
        /// The unchanged replay floor.
        since: u64,
        /// The inclusive bound for this page.
        until: u64,
    },
    /// The stored backlog is drained; only live traffic remains. Call
    /// [`FeedTransport::close_backfill`], which is a no-op when no further
    /// paging was ever needed.
    ReplayComplete {
        /// The walk stopped before reaching the floor — see
        /// [`ReplayStep::Complete`]. The events behind the cut are gone for
        /// good, so this is an operational alert, not a debug detail.
        truncated: bool,
    },
    /// The membership-change watch closed. This is the feed's connection
    /// health signal now — see the module docs — so the caller should
    /// reconnect, which re-runs the whole discover/subscribe sequence.
    Closed(String),
    /// The relay closed one channel's live subscription — access revoked,
    /// rate limited, or similar — without necessarily being preceded by a
    /// membership-removed notification. Not a reconnect: the caller repairs
    /// this one subscription and keeps the socket.
    ///
    /// *How* it repairs depends on `reason`, and the distinction matters. An
    /// ordinary close is an independent failure, so resubscribing at once is
    /// right — left alone, that channel's coverage degrades silently until an
    /// unrelated reconnect or membership event happens to fire. A close that
    /// [`is_rate_limited`] is the opposite: correlated across every channel
    /// and self-perpetuating, because the immediate retry is what re-trips
    /// the quota. Those park for [`rate_limit_park_ms`] instead.
    ChannelLiveClosed {
        /// The channel whose live subscription closed.
        channel_id: Uuid,
        /// The relay's stated reason.
        reason: String,
    },
    /// This agent's channel membership changed. Call
    /// [`FeedTransport::subscribe_channel_live`] (`added: true`) or
    /// [`FeedTransport::unsubscribe_channel_live`] (`added: false`) for
    /// `channel_id`.
    ChannelMembershipChanged {
        /// The channel that changed.
        channel_id: Uuid,
        /// `true` for an add, `false` for a remove.
        added: bool,
    },
}

/// Fold one frame into the cursor and say what the caller should do with it.
///
/// Kept separate from the connection loop so the interesting half — what
/// happens to the cursor for each kind of frame — is exercised without a
/// socket.
///
/// `replay` is folded here too, because whether an EOSE ends the replay depends
/// on how many rows the page before it carried — see [`WakeReplay`]. Every row
/// the relay serves on the backfill subscription must reach this function, so
/// pass unparseable and rejected frames as well as good ones; they count
/// against the relay's page just the same. Rows from a live subscription reach
/// this function too — [`WakeReplay::observe_row`] simply does not tally them.
///
/// # Errors
/// Propagates [`crate::cursor::CursorError`] from the durable claim. A cursor
/// that cannot be made durable is fatal by design: continuing would process
/// events the next restart has no record of.
pub fn step(
    cursor: &mut CursorStore,
    replay: &mut WakeReplay,
    frame: &FeedFrame,
    now: u64,
) -> Result<FeedStep, crate::cursor::CursorError> {
    match classify(frame) {
        FeedSignal::Trigger { sub, event } => {
            replay.observe_row(sub, Some(event.created_at));
            let admission = cursor.admit(&event.id, event.created_at, now)?;
            match admission {
                Admission::Fresh => Ok(FeedStep::Admitted {
                    event,
                    undeliverable: false,
                }),
                Admission::FreshButUndeliverable { .. } => Ok(FeedStep::Admitted {
                    event,
                    undeliverable: true,
                }),
                Admission::Duplicate => Ok(FeedStep::Nothing),
            }
        }
        FeedSignal::Unparsed { sub } => {
            replay.observe_row(sub, None);
            Ok(FeedStep::Nothing)
        }
        // A forged event's `created_at` is as unbelievable as its author, so it
        // contributes no paging bound — but it did occupy a row of the page.
        FeedSignal::Rejected {
            sub,
            event_id,
            reason,
        } => {
            replay.observe_row(sub, None);
            Ok(FeedStep::Rejected { event_id, reason })
        }
        FeedSignal::ReplayComplete { sub } => match replay.on_eose(sub) {
            Some(ReplayStep::Backfill { since, until }) => Ok(FeedStep::Backfill { since, until }),
            // Coverage advances even on a truncated replay. Leaving the pin
            // held would be the worse failure: it never clears by itself, so
            // the checkpoint would stay stuck at this moment for the life of
            // the process and every later restart would report `GapTooOld`.
            // Ending it and saying so keeps the loss bounded and visible.
            // (Alex settled this in round 3, on condition the daemon treats
            // `truncated` as an operational incident.)
            Some(ReplayStep::Complete { truncated }) => {
                cursor.end_replay(now)?;
                Ok(FeedStep::ReplayComplete { truncated })
            }
            // An EOSE that does not move the replay: any live subscription's
            // EOSE (never gates replay — see the module docs), the
            // membership watch's EOSE, or a backfill one after the walk
            // finished.
            None => Ok(FeedStep::Nothing),
        },
        // Only the membership watch going away is a reconnect — it is the
        // feed's connection-health signal now that live fan-out is split
        // across N per-channel subscriptions. The backfill subscription is
        // expendable: the caller closes it itself on every completion, and
        // the relay answers that with a CLOSED — treating it as fatal would
        // make a clean replay reconnect forever. Losing one channel's live
        // subscription costs only that channel, not the whole feed.
        FeedSignal::Closed {
            sub: WakeSubscription::Membership,
            reason,
        } => Ok(FeedStep::Closed(reason)),
        FeedSignal::Closed {
            sub: WakeSubscription::Backfill,
            ..
        } => match replay.on_backfill_closed() {
            Some(ReplayStep::Complete { truncated }) => {
                cursor.end_replay(now)?;
                Ok(FeedStep::ReplayComplete { truncated })
            }
            _ => Ok(FeedStep::Nothing),
        },
        FeedSignal::Closed {
            sub: WakeSubscription::Live(channel_id),
            reason,
        } => Ok(FeedStep::ChannelLiveClosed { channel_id, reason }),
        FeedSignal::MembershipChanged { channel_id, change } => {
            Ok(FeedStep::ChannelMembershipChanged {
                channel_id,
                added: matches!(change, MembershipChangeKind::Added),
            })
        }
        FeedSignal::Ignored => Ok(FeedStep::Nothing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_core::kind::{
        KIND_MEMBER_ADDED_NOTIFICATION, KIND_MEMBER_REMOVED_NOTIFICATION, KIND_REACTION,
        KIND_STREAM_MESSAGE,
    };

    fn event_json_at(id: &str, kind: u32, p_tags: &[&str], created_at: u64) -> Value {
        json!({
            "id": id,
            "pubkey": "aa".repeat(32),
            "kind": kind,
            "created_at": created_at,
            "tags": p_tags
                .iter()
                .map(|p| json!(["p", p]))
                .collect::<Vec<_>>(),
            "content": "hello",
        })
    }

    fn event_json(id: &str, kind: u32, p_tags: &[&str]) -> Value {
        event_json_at(id, kind, p_tags, 1_700_000_000)
    }

    fn membership_event(id: &str, kind: u32, channel_id: Uuid, agent_p: &str) -> Value {
        json!({
            "id": id,
            "pubkey": "aa".repeat(32),
            "kind": kind,
            "created_at": 1_700_000_000_u64,
            "tags": [["h", channel_id.to_string()], ["p", agent_p]],
            "content": "",
        })
    }

    fn a_channel() -> Uuid {
        Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888)
    }

    #[test]
    fn the_filter_always_names_kinds() {
        // Without `kinds` the relay's p-gate answers 403, which reads as an
        // auth failure rather than a bad filter. This is the guard for that.
        let filter = wake_filter(&"ab".repeat(32), 42, None, None);
        let kinds = filter["kinds"].as_array().expect("kinds must be present");
        assert_eq!(kinds.len(), WAKE_TRIGGER_KINDS.len());
        assert_eq!(filter["since"], 42);
    }

    #[test]
    fn the_filter_names_exactly_one_agent() {
        // The authorization boundary, pinned as a type-level property rather
        // than a convention: the relay scopes history and private-channel
        // fan-out to the *connection's* pubkey, so a second `#p` value in one
        // filter is a mention that silently never arrives. `wake_filter` takes
        // one pubkey and there is no overload that takes a list.
        let filter = wake_filter(&format!("  {}  ", "AB".repeat(32)), 0, None, None);
        let targets = filter["#p"].as_array().expect("#p must be present");
        assert_eq!(targets.len(), 1, "one feed watches exactly one agent");
        assert_eq!(
            targets[0],
            "ab".repeat(32),
            "the pubkey is normalized to the one comparison form"
        );
    }

    #[test]
    fn the_filter_bounds_a_page_only_when_paging() {
        // `until` is what makes a filter historical, and a historical filter
        // carries no live traffic — so the unbounded form must not grow one by
        // accident.
        let live = wake_filter(&"ab".repeat(32), 10, None, None);
        assert!(
            live.get("until").is_none(),
            "the unbounded form must not be bounded"
        );
        assert_eq!(live["limit"], REPLAY_PAGE_LIMIT);

        let page = wake_filter(&"ab".repeat(32), 10, Some(99), None);
        assert_eq!(page["until"], 99);
        assert_eq!(page["since"], 10, "the floor never moves while paging");
    }

    #[test]
    fn the_filter_adds_h_only_when_channel_scoped() {
        let global = wake_filter(&"ab".repeat(32), 10, None, None);
        assert!(
            global.get("#h").is_none(),
            "the backfill shape must stay #h-less — it is access-scoped server-side"
        );

        let ch = a_channel();
        let scoped = wake_filter(&"ab".repeat(32), 10, None, Some(ch));
        assert_eq!(scoped["#h"], json!([ch.to_string()]));
    }

    /// Alex's review finding on `2aa51a6e6`, pinned as a regression against the
    /// exact matching logic the relay runs (`buzz_core::filter::filters_match`,
    /// not a stand-in): a per-channel live REQ bound to connect time silently
    /// drops a mention whose signed `created_at` lands a few seconds behind
    /// wall-clock now — ordinary clock skew or a queue-then-publish delay, both
    /// within the relay's accepted drift, not malicious backdating. If that
    /// timestamp also falls behind wherever the backfill walk has already
    /// paged to, the event is unrecoverable: backfill never revisits a window
    /// it has moved past. Binding `since` to the cursor's replay floor instead
    /// — the same floor [`WakeReplay::since`] holds and `subscribe_backfill`
    /// uses — closes it, because the floor sits far enough in the past that
    /// ordinary drift can never cross it.
    #[test]
    fn a_replay_floor_since_would_have_caught_a_delayed_event_that_connect_time_would_miss() {
        // Two distinct identities: a mention is authored by someone other
        // than the agent it addresses. Using the same key for both would let
        // the nostr crate's own self-mention dedup silently drop the `#p` tag
        // before it ever reaches the filter, masking the thing this test
        // exists to prove.
        let author_keys = nostr::Keys::generate();
        let agent_keys = nostr::Keys::generate();
        let agent_pubkey = agent_keys.public_key().to_hex();
        let ch = a_channel();

        let now: u64 = 2_000_000_000;
        // A few seconds behind "now" — within accepted drift, not a forgery.
        let event_created_at = now - 3;

        let event =
            nostr::EventBuilder::new(nostr::Kind::Custom(WAKE_TRIGGER_KINDS[1] as u16), "hello")
                .tags([
                    nostr::Tag::parse(["h", &ch.to_string()]).expect("valid h tag"),
                    nostr::Tag::parse(["p", &agent_pubkey]).expect("valid p tag"),
                ])
                .custom_created_at(nostr::Timestamp::from(event_created_at))
                .sign_with_keys(&author_keys)
                .expect("signs");
        let stored = buzz_core::StoredEvent::new(event, Some(ch));

        // The cursor's replay floor: comfortably behind `now`, the way
        // `CursorStore::resume` (already overlap-adjusted) actually produces
        // it — never anywhere near connect time.
        let floor = now - 10_000;

        let floor_filter: nostr::Filter =
            serde_json::from_value(wake_filter(&agent_pubkey, floor, None, Some(ch)))
                .expect("wake_filter always produces a valid NIP-01 filter");
        assert!(
            buzz_core::filter::filters_match(&[floor_filter], &stored),
            "a live subscription bound to the cursor's replay floor must catch \
             a delayed event even though its created_at sits behind wall-clock now"
        );

        // The rejected design, pinned so a regression back to it fails loudly.
        let connect_time_filter: nostr::Filter =
            serde_json::from_value(wake_filter(&agent_pubkey, now, None, Some(ch)))
                .expect("wake_filter always produces a valid NIP-01 filter");
        assert!(
            !buzz_core::filter::filters_match(&[connect_time_filter], &stored),
            "connect-time `since` is exactly the gap Alex's review caught — a \
             live subscription bound to it silently drops this event"
        );
    }

    #[test]
    fn live_req_ids_are_deterministic_per_channel() {
        // Deterministic, not random: a reconnect (or a redelivered
        // membership-added notification for a channel already tracked)
        // replaces the existing subscription instead of accumulating a
        // second one.
        let ch = a_channel();
        let req = wake_live_req(&"ab".repeat(32), ch, 100);
        assert_eq!(req[1], wake_live_subscription_id(ch));
        let req_again = wake_live_req(&"ab".repeat(32), ch, 200);
        assert_eq!(
            req[1], req_again[1],
            "the same channel must always yield the same subscription id"
        );
    }

    #[test]
    fn wake_subscription_parses_every_id_it_produces() {
        let ch = a_channel();
        assert_eq!(
            WakeSubscription::from_id(&wake_live_subscription_id(ch)),
            Some(WakeSubscription::Live(ch))
        );
        assert_eq!(
            WakeSubscription::from_id(WAKE_BACKFILL_SUBSCRIPTION_ID),
            Some(WakeSubscription::Backfill)
        );
        assert_eq!(
            WakeSubscription::from_id(WAKE_MEMBERSHIP_SUBSCRIPTION_ID),
            Some(WakeSubscription::Membership)
        );
        assert_eq!(
            WakeSubscription::from_id("someone-elses"),
            None,
            "a foreign subscription id must not resolve to any of ours"
        );
        assert_eq!(
            WakeSubscription::from_id("buzz-waker-live-not-a-uuid"),
            None,
            "a malformed channel suffix must not resolve to a channel"
        );
    }

    #[test]
    fn an_event_on_another_subscription_is_ignored() {
        // One connection can carry more than one subscription. Acting on a
        // frame from a filter this module never wrote would wake agents for
        // reasons no test here covers.
        let frame = FeedFrame::Event {
            subscription_id: "someone-elses".to_string(),
            event: event_json("ff".repeat(32).as_str(), KIND_STREAM_MESSAGE, &[]),
        };
        assert_eq!(classify(&frame), FeedSignal::Ignored);
    }

    #[test]
    fn an_unreadable_event_on_our_subscription_is_not_the_same_as_a_foreign_frame() {
        // Both end up waking nobody, but only one of them consumed a row of
        // the relay's page — and the page count is what decides whether the
        // replay keeps going.
        let mut broken = event_json(&"ff".repeat(32), KIND_STREAM_MESSAGE, &[]);
        broken.as_object_mut().expect("object").remove("created_at");
        let frame = FeedFrame::Event {
            subscription_id: WAKE_BACKFILL_SUBSCRIPTION_ID.to_string(),
            event: broken,
        };
        assert_eq!(
            classify(&frame),
            FeedSignal::Unparsed {
                sub: WakeSubscription::Backfill
            }
        );
    }

    #[test]
    fn a_structurally_broken_event_is_ignored_not_defaulted() {
        for missing in ["id", "pubkey", "kind", "created_at"] {
            let mut event = event_json(&"ff".repeat(32), KIND_STREAM_MESSAGE, &[]);
            event.as_object_mut().expect("object").remove(missing);
            assert!(
                trigger_event_from_json(&event).is_none(),
                "an event missing {missing} must be refused, never defaulted"
            );
        }
    }

    #[test]
    fn only_p_tags_are_carried_off_the_wire() {
        let event = json!({
            "id": "ff".repeat(32),
            "pubkey": "aa".repeat(32),
            "kind": KIND_STREAM_MESSAGE,
            "created_at": 1_700_000_000_u64,
            "tags": [
                ["h", "channel-uuid"],
                ["p", "bb".repeat(32)],
                ["e", "cc".repeat(32), "", "reply"],
                ["p", "dd".repeat(32)],
            ],
        });
        let trigger = trigger_event_from_json(&event).expect("well-formed event");
        assert_eq!(trigger.p_tags, vec!["bb".repeat(32), "dd".repeat(32)]);
    }

    #[test]
    fn a_non_trigger_kind_still_parses_and_is_refused_downstream() {
        // The feed does not filter by kind a second time — `select_wake_candidates`
        // owns that gate. Parsing here must not silently drop the event, or the
        // reason it was refused would be invisible.
        let event = event_json(&"ff".repeat(32), KIND_REACTION, &[]);
        let trigger = trigger_event_from_json(&event).expect("parses");
        assert_eq!(trigger.kind, KIND_REACTION);
    }

    #[test]
    fn a_membership_added_event_names_its_channel() {
        let ch = a_channel();
        let event = membership_event(
            &"ff".repeat(32),
            KIND_MEMBER_ADDED_NOTIFICATION,
            ch,
            &"bb".repeat(32),
        );
        assert_eq!(
            membership_change_from_json(&event),
            Some((ch, MembershipChangeKind::Added))
        );
    }

    #[test]
    fn a_membership_removed_event_names_its_channel() {
        let ch = a_channel();
        let event = membership_event(
            &"ff".repeat(32),
            KIND_MEMBER_REMOVED_NOTIFICATION,
            ch,
            &"bb".repeat(32),
        );
        assert_eq!(
            membership_change_from_json(&event),
            Some((ch, MembershipChangeKind::Removed))
        );
    }

    #[test]
    fn a_membership_event_without_an_h_tag_is_refused_not_defaulted() {
        let event = json!({
            "id": "ff".repeat(32),
            "pubkey": "aa".repeat(32),
            "kind": KIND_MEMBER_ADDED_NOTIFICATION,
            "created_at": 1_700_000_000_u64,
            "tags": [["p", "bb".repeat(32)]],
        });
        assert_eq!(membership_change_from_json(&event), None);
    }

    #[test]
    fn a_membership_frame_classifies_distinctly_from_a_trigger() {
        let ch = a_channel();
        let frame = FeedFrame::Event {
            subscription_id: WAKE_MEMBERSHIP_SUBSCRIPTION_ID.to_string(),
            event: membership_event(
                &"ff".repeat(32),
                KIND_MEMBER_ADDED_NOTIFICATION,
                ch,
                &"bb".repeat(32),
            ),
        };
        assert_eq!(
            classify(&frame),
            FeedSignal::MembershipChanged {
                channel_id: ch,
                change: MembershipChangeKind::Added
            }
        );
    }

    fn cursor_at(dir: &tempfile::TempDir, now: u64) -> CursorStore {
        CursorStore::open_or_start(
            dir.path().join("cursor.json"),
            now,
            crate::cursor::DEFAULT_COMPLETED_RING,
        )
        .expect("a fresh cursor opens")
    }

    #[test]
    fn a_replayed_duplicate_is_claimed_once() {
        // The reconnect overlap re-delivers events on purpose, so the same
        // mention arriving twice is the normal case, not an anomaly. Waking
        // twice for it would spend a second deploy on an agent that is already
        // being started.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 1_700_000_000;
        let mut cursor = cursor_at(&dir, now);
        let frame = FeedFrame::Event {
            subscription_id: WAKE_BACKFILL_SUBSCRIPTION_ID.to_string(),
            event: event_json(
                &"ff".repeat(32),
                KIND_STREAM_MESSAGE,
                &["bb".repeat(32).as_str()],
            ),
        };

        let mut replay = WakeReplay::new(now);
        let first = step(&mut cursor, &mut replay, &frame, now).expect("first admission");
        assert!(
            matches!(first, FeedStep::Admitted { .. }),
            "the first delivery is the one that does the work"
        );
        let second = step(&mut cursor, &mut replay, &frame, now).expect("second admission");
        assert!(
            matches!(second, FeedStep::Nothing),
            "the same event delivered again must not be claimed twice"
        );
    }

    #[test]
    fn a_fresh_replay_starts_walking_with_no_channels_discovered() {
        // Alex: "verify an initially empty channel set does not suppress the
        // global backfill." The replay never looks at channel count at all —
        // it starts in the backfill phase unconditionally — so this holds by
        // construction. Pinned here as a behavioural test, not just a
        // property of the types: a short first page must be able to complete
        // the walk with zero live frames ever having been processed.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 1_700_000_000;
        let mut cursor = cursor_at(&dir, now);
        let mut replay = WakeReplay::new(now);
        assert!(
            !replay.is_complete(),
            "the walk must already be in progress at construction"
        );
        let _ = cursor.resume(now);

        let step_result = step(
            &mut cursor,
            &mut replay,
            &eose(WAKE_BACKFILL_SUBSCRIPTION_ID),
            now + 1,
        )
        .expect("folds");
        assert!(matches!(
            step_result,
            FeedStep::ReplayComplete { truncated: false }
        ));
    }

    #[test]
    fn eose_on_a_short_page_ends_the_replay_and_advances_coverage() {
        // EOSE on an *unclamped* page is the proof that the stored backlog is
        // drained, which is what lets the checkpoint stop being pinned to the
        // replay floor. The qualifier matters — see the paging tests below for
        // the EOSE that proves nothing.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 1_700_000_000;
        let mut cursor = cursor_at(&dir, now);
        let mut replay = WakeReplay::new(now);
        // The pin only exists while a replay is draining, and `resume` is what
        // sets it — the loop's own order: resume, REQ, then EOSE.
        let _ = cursor.resume(now);
        let before = cursor.state().covered_through_secs;

        let step_result = step(
            &mut cursor,
            &mut replay,
            &FeedFrame::Eose {
                subscription_id: WAKE_BACKFILL_SUBSCRIPTION_ID.to_string(),
            },
            now + 30,
        )
        .expect("end_replay persists");

        assert!(matches!(
            step_result,
            FeedStep::ReplayComplete { truncated: false }
        ));
        assert!(
            cursor.state().covered_through_secs > before,
            "EOSE is evidence the feed is current, so coverage must advance"
        );
    }

    #[test]
    fn a_clamped_first_page_starts_a_bounded_second_page() {
        // The relay clamps every historical filter to a page and orders it
        // newest-first, so EOSE after a full page means "this page is
        // drained", not "history is". Releasing the cursor's replay pin there
        // skips the rest permanently: the request is deterministic, so
        // repeating it returns the same newest page.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 2_000_000_000;
        let mut cursor = cursor_at(&dir, now);
        let mut replay = WakeReplay::new(now - 10_000);
        let _ = cursor.resume(now);
        let covered_before = cursor.state().covered_through_secs;

        deliver_page(
            &mut cursor,
            &mut replay,
            WAKE_BACKFILL_SUBSCRIPTION_ID,
            REPLAY_PAGE_LIMIT,
            |row| now - u64::from(row),
            0,
        );

        let oldest = now - u64::from(REPLAY_PAGE_LIMIT - 1);
        match step(
            &mut cursor,
            &mut replay,
            &eose(WAKE_BACKFILL_SUBSCRIPTION_ID),
            now,
        )
        .expect("folds")
        {
            FeedStep::Backfill { since, until } => {
                assert_eq!(since, now - 10_000, "the floor must not move while paging");
                assert_eq!(until, oldest, "the walk starts at this page's oldest row");
            }
            other => panic!("a clamped first page has history behind it, got {other:?}"),
        }
        assert_eq!(
            cursor.state().covered_through_secs,
            covered_before,
            "coverage must not advance while the backlog is still draining"
        );

        // A short second page drains the backlog.
        deliver_page(
            &mut cursor,
            &mut replay,
            WAKE_BACKFILL_SUBSCRIPTION_ID,
            2,
            |row| oldest - u64::from(row),
            9_000,
        );
        match step(
            &mut cursor,
            &mut replay,
            &eose(WAKE_BACKFILL_SUBSCRIPTION_ID),
            now + 5,
        )
        .expect("folds")
        {
            FeedStep::ReplayComplete { truncated: false } => {}
            other => panic!("a short backfill page ends the walk, got {other:?}"),
        }
        assert!(
            cursor.state().covered_through_secs > covered_before,
            "coverage advances once, at the end of the whole replay"
        );
    }

    #[test]
    fn mentions_arriving_on_a_live_subscription_are_admitted_and_never_tallied() {
        // The finding this pins: a live subscription's rows must reach the
        // cursor (they are real mentions) but must never count toward the
        // backfill page tally (they carry no historical component and are
        // not evidence of a clamp).
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 2_000_000_000;
        let mut cursor = cursor_at(&dir, now);
        let mut replay = WakeReplay::new(now - 10_000);
        let _ = cursor.resume(now);

        deliver_page(
            &mut cursor,
            &mut replay,
            WAKE_BACKFILL_SUBSCRIPTION_ID,
            REPLAY_PAGE_LIMIT,
            |row| now - u64::from(row),
            0,
        );
        let bound = match step(
            &mut cursor,
            &mut replay,
            &eose(WAKE_BACKFILL_SUBSCRIPTION_ID),
            now,
        )
        .expect("folds")
        {
            FeedStep::Backfill { until, .. } => until,
            other => panic!("expected a backfill, got {other:?}"),
        };

        // While the bounded page is standing, more than a page of new
        // mentions is delivered live — all newer than the bound, so no
        // backfill page can carry them.
        let ch = a_channel();
        let arrivals = REPLAY_PAGE_LIMIT + 1;
        let mut admitted = 0_u32;
        for row in 0..arrivals {
            let frame = FeedFrame::Event {
                subscription_id: wake_live_subscription_id(ch),
                event: event_json_at(
                    &format!("{:064x}", 500_000 + row),
                    KIND_STREAM_MESSAGE,
                    &["bb".repeat(32).as_str()],
                    now + 1 + u64::from(row),
                ),
            };
            if matches!(
                step(&mut cursor, &mut replay, &frame, now).expect("admits"),
                FeedStep::Admitted { .. }
            ) {
                admitted += 1;
            }
        }
        assert_eq!(
            admitted, arrivals,
            "every mention delivered live during the walk must be admitted, \
             including the {arrivals}th that no capped historical page could \
             have recovered"
        );

        // The live arrivals are not a page: tallying them would look like a
        // clamp and send the walk after history it has already covered.
        deliver_page(
            &mut cursor,
            &mut replay,
            WAKE_BACKFILL_SUBSCRIPTION_ID,
            2,
            |row| bound - u64::from(row),
            9_000,
        );
        match step(
            &mut cursor,
            &mut replay,
            &eose(WAKE_BACKFILL_SUBSCRIPTION_ID),
            now + 5,
        )
        .expect("folds")
        {
            FeedStep::ReplayComplete { truncated: false } => {}
            other => panic!("the walk drained cleanly, got {other:?}"),
        }
    }

    #[test]
    fn a_live_subscriptions_own_eose_never_gates_replay() {
        // Alex: live REQs' EOSEs must not gate replay. A live subscription's
        // EOSE fires almost immediately (its filter's `since` is connect
        // time, so there is nothing behind it) and must be inert here.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 1_700_000_000;
        let mut cursor = cursor_at(&dir, now);
        let mut replay = WakeReplay::new(now);
        let ch = a_channel();

        let step_result = step(
            &mut cursor,
            &mut replay,
            &eose(&wake_live_subscription_id(ch)),
            now,
        )
        .expect("folds");
        assert!(
            matches!(step_result, FeedStep::Nothing),
            "a live subscription's EOSE must not move the replay, got {step_result:?}"
        );
        assert!(
            !replay.is_complete(),
            "the backfill walk this connection started with must still be open"
        );
    }

    #[test]
    fn closing_the_backfill_is_not_a_reconnect() {
        // The caller closes the backfill subscription on every completion, and
        // the relay answers a CLOSE with a CLOSED of its own
        // (`handlers/close.rs`). Reading that echo as a failed subscription
        // would put a perfectly clean replay into a reconnect loop.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 1_700_000_000;
        let mut cursor = cursor_at(&dir, now);
        let mut replay = WakeReplay::new(now);
        let _ = cursor.resume(now);

        // Drain the first page short, so the replay is already complete.
        step(
            &mut cursor,
            &mut replay,
            &eose(WAKE_BACKFILL_SUBSCRIPTION_ID),
            now,
        )
        .expect("folds");
        assert!(replay.is_complete());

        let echo = FeedFrame::Closed {
            subscription_id: WAKE_BACKFILL_SUBSCRIPTION_ID.to_string(),
            message: String::new(),
        };
        match step(&mut cursor, &mut replay, &echo, now).expect("folds") {
            FeedStep::Nothing => {}
            other => panic!("the CLOSE echo must be inert, got {other:?}"),
        }

        // The membership watch going away is still a reconnect.
        let membership_closed = FeedFrame::Closed {
            subscription_id: WAKE_MEMBERSHIP_SUBSCRIPTION_ID.to_string(),
            message: "restricted: not authorized".to_string(),
        };
        match step(&mut cursor, &mut replay, &membership_closed, now).expect("folds") {
            FeedStep::Closed(reason) => assert!(reason.contains("restricted")),
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    #[test]
    fn losing_one_channels_live_subscription_is_not_a_reconnect() {
        // The finding Alex's ordering invariant protects against: N
        // per-channel live subscriptions means losing one must cost only
        // that channel, never the whole feed — unlike the single old live
        // subscription, whose loss really did mean reconnect.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 1_700_000_000;
        let mut cursor = cursor_at(&dir, now);
        let mut replay = WakeReplay::new(now);
        let ch = a_channel();

        let closed = FeedFrame::Closed {
            subscription_id: wake_live_subscription_id(ch),
            message: "restricted: channel access revoked".to_string(),
        };
        match step(&mut cursor, &mut replay, &closed, now).expect("folds") {
            FeedStep::ChannelLiveClosed { channel_id, reason } => {
                assert_eq!(channel_id, ch);
                assert!(reason.contains("revoked"));
            }
            other => panic!("expected ChannelLiveClosed, got {other:?}"),
        }
    }

    #[test]
    fn a_backfill_closed_mid_walk_ends_the_replay_short_rather_than_reconnecting() {
        // Every live subscription is the one that matters; losing the walk
        // costs history, not the feed. Report the cut and carry on.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 2_000_000_000;
        let mut cursor = cursor_at(&dir, now);
        let mut replay = WakeReplay::new(now - 10_000);
        let _ = cursor.resume(now);

        deliver_page(
            &mut cursor,
            &mut replay,
            WAKE_BACKFILL_SUBSCRIPTION_ID,
            REPLAY_PAGE_LIMIT,
            |row| now - u64::from(row),
            0,
        );
        step(
            &mut cursor,
            &mut replay,
            &eose(WAKE_BACKFILL_SUBSCRIPTION_ID),
            now,
        )
        .expect("folds");

        let closed = FeedFrame::Closed {
            subscription_id: WAKE_BACKFILL_SUBSCRIPTION_ID.to_string(),
            message: "error: too many subscriptions".to_string(),
        };
        match step(&mut cursor, &mut replay, &closed, now + 5).expect("folds") {
            FeedStep::ReplayComplete { truncated: true } => {}
            other => panic!("a lost walk is a truncated replay, got {other:?}"),
        }
    }

    #[test]
    fn a_forged_event_is_surfaced_and_never_admitted() {
        // The transport refuses to project an event whose signature does not
        // check out. It still occupied a row of the relay's page, so it counts
        // toward paging — but it contributes no timestamp to page on, because
        // a forged `created_at` is exactly as trustworthy as its forged author.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 1_700_000_000;
        let mut cursor = cursor_at(&dir, now);
        let mut replay = WakeReplay::new(now);

        let step_result = step(
            &mut cursor,
            &mut replay,
            &FeedFrame::Rejected {
                subscription_id: WAKE_BACKFILL_SUBSCRIPTION_ID.to_string(),
                event_id: "ff".repeat(32),
                reason: "invalid signature".to_string(),
            },
            now,
        )
        .expect("rejecting an event touches no durable state");

        match step_result {
            FeedStep::Rejected { reason, .. } => assert!(reason.contains("signature")),
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert_eq!(
            cursor.in_flight_len(),
            0,
            "a forged event must never be claimed"
        );
    }

    fn eose(subscription_id: &str) -> FeedFrame {
        FeedFrame::Eose {
            subscription_id: subscription_id.to_string(),
        }
    }

    /// Feed `rows` events down `subscription_id`, timestamped by `stamp` and
    /// given distinct ids via `salt`, exactly as the relay would serve a page.
    fn deliver_page(
        cursor: &mut CursorStore,
        replay: &mut WakeReplay,
        subscription_id: &str,
        rows: u32,
        stamp: impl Fn(u32) -> u64,
        salt: u32,
    ) {
        for row in 0..rows {
            let frame = FeedFrame::Event {
                subscription_id: subscription_id.to_string(),
                event: event_json_at(
                    &format!("{:064x}", row + salt),
                    KIND_STREAM_MESSAGE,
                    &["bb".repeat(32).as_str()],
                    stamp(row),
                ),
            };
            step(cursor, replay, &frame, 2_000_000_000).expect("admits");
        }
    }

    #[test]
    fn paging_stops_rather_than_looping_on_a_tied_second() {
        // `until` is inclusive. If more than a page of events share one second,
        // the next page's oldest row is the bound we just asked for and no
        // progress is possible. Re-requesting forever would hold the feed in
        // replay and answer nobody, so it stops and reports the cut.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 2_000_000_000;
        let mut cursor = cursor_at(&dir, now);
        let mut replay = WakeReplay::new(now - 10_000);
        let _ = cursor.resume(now);

        deliver_page(
            &mut cursor,
            &mut replay,
            WAKE_BACKFILL_SUBSCRIPTION_ID,
            REPLAY_PAGE_LIMIT,
            |_| now - 1,
            0,
        );
        match step(
            &mut cursor,
            &mut replay,
            &eose(WAKE_BACKFILL_SUBSCRIPTION_ID),
            now,
        )
        .expect("folds")
        {
            FeedStep::Backfill { until, .. } => assert_eq!(until, now - 1),
            other => panic!("expected a backfill, got {other:?}"),
        }

        // The backfill page is the same second again — `until` cannot advance.
        deliver_page(
            &mut cursor,
            &mut replay,
            WAKE_BACKFILL_SUBSCRIPTION_ID,
            REPLAY_PAGE_LIMIT,
            |_| now - 1,
            REPLAY_PAGE_LIMIT,
        );
        match step(
            &mut cursor,
            &mut replay,
            &eose(WAKE_BACKFILL_SUBSCRIPTION_ID),
            now + 5,
        )
        .expect("folds")
        {
            FeedStep::ReplayComplete { truncated: true } => {}
            other => panic!("a stalled walk must report the cut, got {other:?}"),
        }
    }

    #[test]
    fn a_closed_membership_watch_is_not_mistaken_for_a_quiet_one() {
        // CLOSED and idle look alike from the loop's perspective and must not:
        // one means reconnect, the other means checkpoint and keep waiting.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 1_700_000_000;
        let mut cursor = cursor_at(&dir, now);
        let mut replay = WakeReplay::new(now);

        let step_result = step(
            &mut cursor,
            &mut replay,
            &FeedFrame::Closed {
                subscription_id: WAKE_MEMBERSHIP_SUBSCRIPTION_ID.to_string(),
                message: "restricted: not authorized".to_string(),
            },
            now,
        )
        .expect("classifying a close touches no durable state");

        match step_result {
            FeedStep::Closed(reason) => assert!(reason.contains("restricted")),
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    #[test]
    fn a_membership_added_signal_tells_the_caller_to_subscribe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 1_700_000_000;
        let mut cursor = cursor_at(&dir, now);
        let mut replay = WakeReplay::new(now);
        let ch = a_channel();

        let frame = FeedFrame::Event {
            subscription_id: WAKE_MEMBERSHIP_SUBSCRIPTION_ID.to_string(),
            event: membership_event(
                &"ff".repeat(32),
                KIND_MEMBER_ADDED_NOTIFICATION,
                ch,
                &"bb".repeat(32),
            ),
        };
        match step(&mut cursor, &mut replay, &frame, now).expect("folds") {
            FeedStep::ChannelMembershipChanged { channel_id, added } => {
                assert_eq!(channel_id, ch);
                assert!(added);
            }
            other => panic!("expected ChannelMembershipChanged, got {other:?}"),
        }
    }

    #[test]
    fn a_membership_removed_signal_tells_the_caller_to_unsubscribe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 1_700_000_000;
        let mut cursor = cursor_at(&dir, now);
        let mut replay = WakeReplay::new(now);
        let ch = a_channel();

        let frame = FeedFrame::Event {
            subscription_id: WAKE_MEMBERSHIP_SUBSCRIPTION_ID.to_string(),
            event: membership_event(
                &"ff".repeat(32),
                KIND_MEMBER_REMOVED_NOTIFICATION,
                ch,
                &"bb".repeat(32),
            ),
        };
        match step(&mut cursor, &mut replay, &frame, now).expect("folds") {
            FeedStep::ChannelMembershipChanged { channel_id, added } => {
                assert_eq!(channel_id, ch);
                assert!(!added);
            }
            other => panic!("expected ChannelMembershipChanged, got {other:?}"),
        }
    }

    #[test]
    fn the_reconnect_ladder_climbs_then_holds() {
        assert_eq!(reconnect_delay_ms(0), 0, "no failures, no wait");
        assert_eq!(reconnect_delay_ms(1), RECONNECT_BASE_DELAY_MS);
        assert_eq!(reconnect_delay_ms(2), RECONNECT_BASE_DELAY_MS * 2);
        assert_eq!(reconnect_delay_ms(3), RECONNECT_BASE_DELAY_MS * 4);
        assert_eq!(reconnect_delay_ms(30), RECONNECT_MAX_DELAY_MS);
        assert_eq!(
            reconnect_delay_ms(u32::MAX),
            RECONNECT_MAX_DELAY_MS,
            "the ladder must not overflow into a short delay"
        );
    }

    // ── Rate-limited channel closes ──────────────────────────────────────────

    /// The close this daemon actually received from the relay, verbatim.
    const OBSERVED_CLOSE: &str = "rate-limited: quota exceeded; retry in 0s";

    #[test]
    fn rate_limited_closes_are_told_apart_by_the_nip01_prefix() {
        assert!(is_rate_limited(OBSERVED_CLOSE));
        assert!(is_rate_limited(
            "rate-limited: too many concurrent requests"
        ));
        assert!(
            !is_rate_limited("restricted: not a member"),
            "an access denial is an independent failure and must still be \
             repaired immediately"
        );
        assert!(
            !is_rate_limited("error: the relay mentioned rate-limited: late"),
            "the prefix is machine-readable precisely so prose cannot trip it"
        );
    }

    #[test]
    fn the_retry_hint_is_read_when_present() {
        assert_eq!(
            parse_rate_limit_retry_secs("rate-limited: quota exceeded; retry in 12s"),
            Some(12)
        );
        assert_eq!(parse_rate_limit_retry_secs(OBSERVED_CLOSE), Some(0));
        assert_eq!(
            parse_rate_limit_retry_secs("rate-limited: too many concurrent requests"),
            None
        );
    }

    /// The regression: the relay says `retry in 0s`, and honouring that
    /// literally is what produced 100 re-subscribes in 25 seconds.
    #[test]
    fn a_zero_second_hint_never_parks_for_zero() {
        let hint = parse_rate_limit_retry_secs(OBSERVED_CLOSE);
        assert_eq!(hint, Some(0), "the relay really does send a literal zero");

        let parked = rate_limit_park_ms(hint, 0);
        assert_ne!(parked, 0, "a zero park is the hot loop this fix exists for");
        assert_eq!(parked, RATE_LIMIT_FLOOR_SECS * 1_000);
    }

    #[test]
    fn short_and_absent_hints_fall_back_to_the_floor() {
        let floor_ms = RATE_LIMIT_FLOOR_SECS * 1_000;
        assert_eq!(rate_limit_park_ms(None, 0), floor_ms, "no hint at all");
        assert_eq!(rate_limit_park_ms(Some(1), 0), floor_ms, "below the min");
        assert_eq!(
            rate_limit_park_ms(Some(RATE_LIMIT_MIN_HINT_SECS), 0),
            RATE_LIMIT_MIN_HINT_SECS * 1_000,
            "a hint at the minimum is trusted as written"
        );
        assert_eq!(rate_limit_park_ms(Some(12), 0), 12_000, "a real hint wins");
    }

    #[test]
    fn the_park_ladder_climbs_then_holds() {
        let floor_ms = RATE_LIMIT_FLOOR_SECS * 1_000;
        assert_eq!(rate_limit_park_ms(Some(0), 0), floor_ms, "first park");
        assert_eq!(rate_limit_park_ms(Some(0), 1), floor_ms * 2);
        assert_eq!(rate_limit_park_ms(Some(0), 2), floor_ms * 4);
        assert_eq!(
            rate_limit_park_ms(Some(0), 30),
            RECONNECT_MAX_DELAY_MS,
            "a persistently rate-limiting relay must settle at the ceiling"
        );
        assert_eq!(
            rate_limit_park_ms(Some(0), u32::MAX),
            RECONNECT_MAX_DELAY_MS,
            "the ladder must not overflow into a short delay"
        );
    }

    /// The P2 regression: a valid hint above the local reconnect ceiling
    /// must not be truncated below it, or the retry lands before the
    /// relay's own reset and can re-trip the same limiter forever.
    #[test]
    fn a_long_hint_is_never_capped_below_itself() {
        assert_eq!(
            rate_limit_park_ms(Some(60), 0),
            60_000,
            "a hint above the reconnect ceiling must win, not get capped to it"
        );
        assert!(
            rate_limit_park_ms(Some(60), 10) >= 60_000,
            "repeated strikes must never bring a valid hint back below itself"
        );
    }
}
