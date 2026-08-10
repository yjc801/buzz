//! The event feed — §4 of `PLANS/BUZZ_WAKER_DESIGN.md`.
//!
//! Everything before this module decides things; this is what gives it
//! something to decide about. It holds one relay subscription for the mentions
//! that address a watched agent, folds each one through the durable cursor, and
//! survives the connection dropping without losing the events that arrived
//! while it was gone.
//!
//! # Shape
//!
//! The same split as [`crate::attempt`]: the policy is pure and the socket is
//! behind [`FeedTransport`]. So the parts that are easy to get wrong — the
//! filter, the reconnect ladder, what each relay frame means — are tested
//! directly, and the transport is a thin adapter with nothing to reason about.
//!
//! # Four things that are not obvious
//!
//! - **The REQ must carry `kinds`.** A filter without them trips the relay's
//!   p-gate and comes back 403 (`CLAUDE.md` § Common Gotchas), which reads as a
//!   permission problem rather than a malformed query. [`WAKE_TRIGGER_KINDS`]
//!   is the right set anyway: [`crate::decide`] discards everything else, so
//!   asking for less traffic costs nothing.
//! - **One feed per agent, bound to that agent's own connection.** See
//!   [`WakeReplay`]. This is §4's option (i)/(ii) as written — the design
//!   already records that "a generic waker identity silently misses
//!   private-channel mentions" — and the type takes one pubkey so a watch list
//!   cannot be folded into one filter by accident.
//! - **EOSE is not the end of history.** The relay clamps every historical
//!   filter to a page and orders it newest-first, so one `since` REQ recovers
//!   only the newest page of an outage. [`WakeReplay`] pages down with `until`
//!   and the caller must not treat replay as complete until it says so.
//! - **The waker connects interactive**, sending no `class` tag. The read-only
//!   connection class exists in this repo's relay but is not deployed on the
//!   relay we use, and the client fails closed when a requested class is not
//!   confirmed — so requesting it would refuse every connection. §4's
//!   deployment amendment records the consequence: a cleanly-exiting watched
//!   agent's presence dot lingers for up to its 180s TTL instead of clearing
//!   at once. Bounded, and the bound holds because a watcher never publishes
//!   presence and so never refreshes that TTL.

use serde_json::{json, Value};

use crate::cursor::{Admission, CursorStore};
use crate::decide::{normalize_pubkey, TriggerEvent, WAKE_TRIGGER_KINDS};

/// Subscription id for the **live** feed: unbounded, opened once per
/// connection, and never re-issued while that connection lasts.
///
/// Fixed rather than random so a reconnect replaces the old subscription
/// instead of accumulating one per attempt, and so relay-side logs stay
/// legible across restarts.
pub const WAKE_LIVE_SUBSCRIPTION_ID: &str = "buzz-waker-live";

/// Subscription id for the **backfill** walk: `until`-bounded, re-issued once
/// per page, and closed when the backlog is drained.
///
/// Separate from [`WAKE_LIVE_SUBSCRIPTION_ID`] for a correctness reason, not a
/// tidiness one — see [`WakeReplay`]. Paging on the live subscription's own id
/// replaces it, and a replaced subscription stops receiving fan-out; every
/// mention published while a bounded filter was standing in its place is then
/// left to be recovered by a later historical query that is itself capped.
pub const WAKE_BACKFILL_SUBSCRIPTION_ID: &str = "buzz-waker-backfill";

/// Which of the feed's two subscriptions a frame arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeSubscription {
    /// The unbounded subscription that stays registered for the connection.
    Live,
    /// The bounded subscription walking the stored backlog.
    Backfill,
}

impl WakeSubscription {
    /// Resolve a relay subscription id, or `None` if it is not one of ours.
    #[must_use]
    pub fn from_id(subscription_id: &str) -> Option<Self> {
        match subscription_id {
            WAKE_LIVE_SUBSCRIPTION_ID => Some(Self::Live),
            WAKE_BACKFILL_SUBSCRIPTION_ID => Some(Self::Backfill),
            _ => None,
        }
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

/// What a relay frame means to the feed.
///
/// Deliberately smaller than the relay's message set: the feed reacts to four
/// things and ignores the rest, and naming the ignored ones here rather than
/// matching them at the call site keeps the loop honest about what it drops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedSignal {
    /// An event on one of our subscriptions that parsed into a candidate
    /// trigger.
    Trigger {
        /// Which subscription delivered it — the tally that drives paging is
        /// per subscription, because the live one keeps delivering long after
        /// its own stored page is drained.
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

/// The REQ filter for **one agent's** wake feed.
///
/// `#p` is the addressing the harness itself keys on, so it is what the feed
/// asks for; `kinds` is mandatory (see the module note); `since` comes from
/// [`CursorStore::resume`], which has already subtracted the reconnect overlap.
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
/// §4 of the design records this as the confirmed constraint behind option (i):
/// per-agent, identity-bound connections. Each watched agent therefore gets its
/// own connection, its own filter, and its own cursor — the replay and coverage
/// state is per identity too, because one agent's EOSE says nothing about
/// another's backlog.
///
/// [`FeedTransport`]'s subscribe methods are the only paths that reach a relay,
/// and they build this from the identity they authenticated with rather than
/// from an argument, so the filter and the connection cannot disagree.
///
/// `until` bounds a backfill page — see [`WakeReplay`]. `None` is the live
/// subscription, which is the only form that carries fan-out.
#[must_use]
pub fn wake_filter(agent_pubkey: &str, since: u64, until: Option<u64>) -> Value {
    let mut filter = json!({
        "kinds": WAKE_TRIGGER_KINDS,
        "#p": [normalize_pubkey(agent_pubkey)],
        "since": since,
        "limit": REPLAY_PAGE_LIMIT,
    });
    if let (Some(until), Some(object)) = (until, filter.as_object_mut()) {
        object.insert("until".to_string(), json!(until));
    }
    filter
}

/// The REQ frame opening one agent's **live** subscription.
///
/// Unbounded, so it is the one that receives fan-out. Issued once per
/// connection and never replaced while that connection lasts.
#[must_use]
pub fn wake_live_req(agent_pubkey: &str, since: u64) -> Value {
    json!([
        "REQ",
        WAKE_LIVE_SUBSCRIPTION_ID,
        wake_filter(agent_pubkey, since, None)
    ])
}

/// The REQ frame for one page of the **backfill** walk.
#[must_use]
pub fn wake_backfill_req(agent_pubkey: &str, since: u64, until: u64) -> Value {
    json!([
        "REQ",
        WAKE_BACKFILL_SUBSCRIPTION_ID,
        wake_filter(agent_pubkey, since, Some(until))
    ])
}

/// The CLOSE frame retiring the backfill subscription once the walk is done.
#[must_use]
pub fn wake_backfill_close() -> Value {
    json!(["CLOSE", WAKE_BACKFILL_SUBSCRIPTION_ID])
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
    /// The stored backlog is drained. Close the backfill subscription; the
    /// live one stays up.
    Complete {
        /// The walk stopped before reaching `since`. Everything older than the
        /// last page it managed to read was never delivered and never will be:
        /// re-issuing the same request returns the same rows.
        truncated: bool,
    },
}

/// Drains the stored backlog one page at a time, **on a subscription of its
/// own** — the thing EOSE alone does not tell you, and the thing one
/// subscription cannot do safely.
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
/// # Why the walk needs its own subscription id
///
/// Re-issuing a REQ under an id **replaces** that subscription, and a replaced
/// subscription stops receiving fan-out. Paging on the live id therefore takes
/// the feed off the air for the whole walk: every mention published while a
/// `until`-bounded filter stands in its place matches no bounded page, and is
/// left to be recovered by the historical query of whatever REQ comes next —
/// which is itself capped at one page. Publish more than a page of mentions
/// during the walk and the oldest of them is neither replayed nor delivered
/// live, and the replay then completes and advances coverage over the hole.
///
/// So there are two subscriptions. [`WAKE_LIVE_SUBSCRIPTION_ID`] is unbounded,
/// opened once, and never replaced — the relay registers a subscription for
/// fan-out *before* running its historical query (`req.rs:239` precedes
/// `req.rs:312`), so from the moment it is opened there is no interval it can
/// miss. [`WAKE_BACKFILL_SUBSCRIPTION_ID`] carries the `until` bounds and is
/// closed when the walk finishes. The live subscription's own stored page is
/// what starts the walk: if it came back clamped, there is history behind it.
///
/// Duplicate deliveries between the two are the normal case, and
/// [`CursorStore::admit`] already collapses them.
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
    /// The live subscription's stored page is still arriving. A clamped page
    /// here is the evidence that starts the backfill walk.
    LiveBacklog,
    /// The backfill subscription is walking down. The live subscription is up
    /// and delivering fan-out throughout, so nothing published during the walk
    /// depends on the walk to find it.
    Backfill {
        /// The inclusive bound the standing backfill page carries.
        until: u64,
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
    /// of the process. The caller opens the live subscription first — that is
    /// the page this begins by draining.
    #[must_use]
    pub fn new(since: u64) -> Self {
        Self {
            since,
            phase: ReplayPhase::LiveBacklog,
            page: PageTally::default(),
            // The caller's live subscribe is page one.
            pages_issued: 1,
        }
    }

    /// The floor every page of this replay asks from.
    #[must_use]
    pub fn since(&self) -> u64 {
        self.since
    }

    /// The bound the standing backfill page carries, or `None` when no backfill
    /// subscription is open.
    #[must_use]
    pub fn backfill_until(&self) -> Option<u64> {
        match self.phase {
            ReplayPhase::Backfill { until } => Some(until),
            ReplayPhase::LiveBacklog | ReplayPhase::Done => None,
        }
    }

    /// Whether the stored backlog is drained and only live traffic remains.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(self.phase, ReplayPhase::Done)
    }

    /// Count one row the relay served, if it belongs to the page currently
    /// draining.
    ///
    /// Rows the live subscription delivers *after* its own EOSE are live
    /// traffic, not a page, and must not be tallied — otherwise a busy channel
    /// during the walk would look like a clamped page and send the walk off
    /// after history that is already covered.
    ///
    /// `created_at` is `None` for a row the feed could not read or would not
    /// believe: it still consumed a row of the relay's page, so it counts, but
    /// it contributes no bound to page on.
    fn observe_row(&mut self, sub: WakeSubscription, created_at: Option<u64>) {
        let counts = matches!(
            (self.phase, sub),
            (ReplayPhase::LiveBacklog, WakeSubscription::Live)
                | (ReplayPhase::Backfill { .. }, WakeSubscription::Backfill)
        );
        if !counts {
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

    /// Fold an EOSE and say whether the backlog is actually drained.
    ///
    /// Returns `None` for an EOSE that does not move the replay — the live
    /// subscription's EOSE arrives once and only its first one matters, and a
    /// backfill EOSE after the walk has finished is not ours to act on.
    pub fn on_eose(&mut self, sub: WakeSubscription) -> Option<ReplayStep> {
        let expected = match self.phase {
            ReplayPhase::LiveBacklog => WakeSubscription::Live,
            ReplayPhase::Backfill { .. } => WakeSubscription::Backfill,
            ReplayPhase::Done => return None,
        };
        if sub != expected {
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
                self.phase = ReplayPhase::Backfill { until: oldest };
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
    /// Not a reconnect: the live subscription is untouched and still the one
    /// that matters, so the feed keeps working and the backlog is simply
    /// reported as cut short. Returns `None` when no walk was in progress —
    /// which is the ordinary case, because the relay answers the CLOSE the
    /// caller sends on completion with a CLOSED of its own
    /// (`handlers/close.rs`), and reading that echo as a failure would put the
    /// feed into a reconnect loop on every clean replay.
    pub fn on_backfill_closed(&mut self) -> Option<ReplayStep> {
        match self.phase {
            ReplayPhase::Backfill { .. } => Some(self.finish(true)),
            ReplayPhase::LiveBacklog | ReplayPhase::Done => None,
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

/// What a frame means to the feed.
///
/// Frames for another subscription id are [`FeedSignal::Ignored`], not an
/// error: one connection may legitimately carry more than one subscription,
/// and silently acting on another's events would wake agents from a filter
/// this module never wrote.
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

/// The socket, behind a trait so the loop above it can be tested.
///
/// One connection, owned for the transport's lifetime. `connect` is separate
/// from construction because a reconnect must be able to rebuild the socket
/// without rebuilding the caller's state — the cursor and the in-flight set
/// have to outlive any one connection.
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

    /// Open the unbounded live subscription under
    /// [`WAKE_LIVE_SUBSCRIPTION_ID`]. Issue this once per connection, before
    /// anything else, and never re-issue it while the connection lasts.
    ///
    /// Every subscribe method here builds its filter from the transport's
    /// **own** authenticated identity; there is deliberately no way to hand one
    /// a pubkey. That is what makes the authorization argument on
    /// [`wake_filter`] hold structurally rather than by convention: the relay
    /// authorizes history and private-channel fan-out against the connection's
    /// pubkey, so a filter naming anyone else is a feed that quietly misses
    /// mentions.
    fn subscribe_live(
        &mut self,
        since: u64,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>>;

    /// Issue one page of the backfill walk under
    /// [`WAKE_BACKFILL_SUBSCRIPTION_ID`].
    ///
    /// Re-issuing replaces the previous page rather than adding a subscription.
    /// It must not share an id with the live subscription: replacing that one
    /// would take the feed off the air for the whole walk — see [`WakeReplay`].
    fn subscribe_backfill(
        &mut self,
        since: u64,
        until: u64,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>>;

    /// Retire the backfill subscription once the walk is done.
    ///
    /// Idempotent by design: the caller sends this on every
    /// [`FeedStep::ReplayComplete`], including the common case where the live
    /// page was short and no backfill was ever opened.
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
    /// **not** complete and the cursor's replay pin stays held. The live
    /// subscription is untouched and keeps delivering throughout.
    Backfill {
        /// The unchanged replay floor.
        since: u64,
        /// The inclusive bound for this page.
        until: u64,
    },
    /// The stored backlog is drained; only live traffic remains. Call
    /// [`FeedTransport::close_backfill`], which is a no-op when no backfill was
    /// ever opened.
    ReplayComplete {
        /// The walk stopped before reaching the floor — see
        /// [`ReplayStep::Complete`]. The events behind the cut are gone for
        /// good, so this is an operational alert, not a debug detail.
        truncated: bool,
    },
    /// The relay closed one of our subscriptions. The caller should reconnect.
    Closed(String),
}

/// Fold one frame into the cursor and say what the caller should do with it.
///
/// Kept separate from the connection loop so the interesting half — what
/// happens to the cursor for each kind of frame — is exercised without a
/// socket.
///
/// `replay` is folded here too, because whether an EOSE ends the replay depends
/// on how many rows the page before it carried — see [`WakeReplay`]. Every row
/// the relay serves on either subscription must reach this function, so pass
/// unparseable and rejected frames as well as good ones; they count against the
/// relay's page just the same.
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
            // An EOSE that does not move the replay: the live subscription's
            // second EOSE, or a backfill one after the walk finished.
            None => Ok(FeedStep::Nothing),
        },
        // Only the live subscription going away is a reconnect. The backfill
        // one is expendable: the caller closes it itself on every completion,
        // and the relay answers that with a CLOSED — treating either as fatal
        // would make a clean replay reconnect forever.
        FeedSignal::Closed {
            sub: WakeSubscription::Live,
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
        FeedSignal::Ignored => Ok(FeedStep::Nothing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_core::kind::{KIND_REACTION, KIND_STREAM_MESSAGE};

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

    #[test]
    fn the_filter_always_names_kinds() {
        // Without `kinds` the relay's p-gate answers 403, which reads as an
        // auth failure rather than a bad filter. This is the guard for that.
        let filter = wake_filter(&"ab".repeat(32), 42, None);
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
        let filter = wake_filter(&format!("  {}  ", "AB".repeat(32)), 0, None);
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
        let live = wake_filter(&"ab".repeat(32), 10, None);
        assert!(
            live.get("until").is_none(),
            "the live subscription must not be bounded"
        );
        assert_eq!(live["limit"], REPLAY_PAGE_LIMIT);

        let page = wake_filter(&"ab".repeat(32), 10, Some(99));
        assert_eq!(page["until"], 99);
        assert_eq!(page["since"], 10, "the floor never moves while paging");
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
            subscription_id: WAKE_LIVE_SUBSCRIPTION_ID.to_string(),
            event: broken,
        };
        assert_eq!(
            classify(&frame),
            FeedSignal::Unparsed {
                sub: WakeSubscription::Live
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
            subscription_id: WAKE_LIVE_SUBSCRIPTION_ID.to_string(),
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
    fn eose_on_a_short_page_ends_the_replay_and_advances_coverage() {
        // EOSE on an *unbounded, unclamped* page is the proof that the stored
        // backlog is drained, which is what lets the checkpoint stop being
        // pinned to the replay floor. The qualifiers matter — see the paging
        // tests below for the EOSE that proves nothing.
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
                subscription_id: WAKE_LIVE_SUBSCRIPTION_ID.to_string(),
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
    fn a_clamped_live_page_starts_a_backfill_on_its_own_subscription() {
        // The relay clamps a historical filter to a page and orders it
        // newest-first, so EOSE after a full page means "this page is drained",
        // not "history is". Releasing the cursor's replay pin there skips the
        // rest permanently: the request is deterministic, so repeating it
        // returns the same newest page.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 2_000_000_000;
        let mut cursor = cursor_at(&dir, now);
        let mut replay = WakeReplay::new(now - 10_000);
        let _ = cursor.resume(now);
        let covered_before = cursor.state().covered_through_secs;

        deliver_page(
            &mut cursor,
            &mut replay,
            WAKE_LIVE_SUBSCRIPTION_ID,
            REPLAY_PAGE_LIMIT,
            |row| now - u64::from(row),
            0,
        );

        let oldest = now - u64::from(REPLAY_PAGE_LIMIT - 1);
        match step(
            &mut cursor,
            &mut replay,
            &eose(WAKE_LIVE_SUBSCRIPTION_ID),
            now,
        )
        .expect("folds")
        {
            FeedStep::Backfill { since, until } => {
                assert_eq!(since, now - 10_000, "the floor must not move while paging");
                assert_eq!(until, oldest, "the walk starts at this page's oldest row");
            }
            other => panic!("a clamped live page has history behind it, got {other:?}"),
        }
        assert_eq!(
            cursor.state().covered_through_secs,
            covered_before,
            "coverage must not advance while the backlog is still draining"
        );

        // A short backfill page drains the backlog. No closing unbounded REQ:
        // the live subscription was never replaced, so the feed is already on
        // live traffic and there is nothing to return to.
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
    fn mentions_arriving_during_the_walk_are_admitted_from_the_live_subscription() {
        // The finding this pins, and the reason there are two subscription ids
        // at all. Re-issuing a REQ under an id *replaces* that subscription and
        // a replaced subscription stops receiving fan-out — so paging on the
        // live id takes the feed off the air for the whole walk. Anything
        // published in that window matches no bounded page, and the next
        // unbounded query is itself capped at one page, so past a page of them
        // the oldest is neither replayed nor delivered. The replay would then
        // complete and advance coverage straight over the hole.
        //
        // With the walk on its own id the live subscription is never replaced,
        // so those mentions arrive as ordinary fan-out while the walk runs.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 2_000_000_000;
        let mut cursor = cursor_at(&dir, now);
        let mut replay = WakeReplay::new(now - 10_000);
        let _ = cursor.resume(now);

        deliver_page(
            &mut cursor,
            &mut replay,
            WAKE_LIVE_SUBSCRIPTION_ID,
            REPLAY_PAGE_LIMIT,
            |row| now - u64::from(row),
            0,
        );
        let bound = match step(
            &mut cursor,
            &mut replay,
            &eose(WAKE_LIVE_SUBSCRIPTION_ID),
            now,
        )
        .expect("folds")
        {
            FeedStep::Backfill { until, .. } => until,
            other => panic!("expected a backfill, got {other:?}"),
        };

        // While the bounded page is standing, more than a page of new mentions
        // is published — all newer than the bound, so no backfill page can
        // carry them.
        let arrivals = REPLAY_PAGE_LIMIT + 1;
        let mut admitted = 0_u32;
        for row in 0..arrivals {
            let frame = FeedFrame::Event {
                subscription_id: WAKE_LIVE_SUBSCRIPTION_ID.to_string(),
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
            "every mention published during the walk must be admitted live, \
             including the {arrivals}th that no capped historical page could \
             have recovered"
        );

        // Those live arrivals are not a page: tallying them would look like a
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

        // Drain the live page short, so the replay is already complete.
        step(
            &mut cursor,
            &mut replay,
            &eose(WAKE_LIVE_SUBSCRIPTION_ID),
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

        // The live subscription going away is still a reconnect.
        let live_closed = FeedFrame::Closed {
            subscription_id: WAKE_LIVE_SUBSCRIPTION_ID.to_string(),
            message: "restricted: not authorized".to_string(),
        };
        match step(&mut cursor, &mut replay, &live_closed, now).expect("folds") {
            FeedStep::Closed(reason) => assert!(reason.contains("restricted")),
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    #[test]
    fn a_backfill_closed_mid_walk_ends_the_replay_short_rather_than_reconnecting() {
        // The live subscription is the one that matters; losing the walk costs
        // history, not the feed. Report the cut and carry on.
        let dir = tempfile::tempdir().expect("tempdir");
        let now = 2_000_000_000;
        let mut cursor = cursor_at(&dir, now);
        let mut replay = WakeReplay::new(now - 10_000);
        let _ = cursor.resume(now);

        deliver_page(
            &mut cursor,
            &mut replay,
            WAKE_LIVE_SUBSCRIPTION_ID,
            REPLAY_PAGE_LIMIT,
            |row| now - u64::from(row),
            0,
        );
        step(
            &mut cursor,
            &mut replay,
            &eose(WAKE_LIVE_SUBSCRIPTION_ID),
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
                subscription_id: WAKE_LIVE_SUBSCRIPTION_ID.to_string(),
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
            WAKE_LIVE_SUBSCRIPTION_ID,
            REPLAY_PAGE_LIMIT,
            |_| now - 1,
            0,
        );
        match step(
            &mut cursor,
            &mut replay,
            &eose(WAKE_LIVE_SUBSCRIPTION_ID),
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
    fn a_closed_subscription_is_not_mistaken_for_a_quiet_one() {
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
                subscription_id: WAKE_LIVE_SUBSCRIPTION_ID.to_string(),
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
}
