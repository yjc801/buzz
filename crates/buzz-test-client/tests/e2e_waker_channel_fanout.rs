//! E2E tests for `buzz-waker`'s per-channel live subscriptions (PR #18, the
//! P1 #1 finding: a single `#p`-only filter can never receive live
//! channel-scoped fan-out — see `buzz-waker/src/feed.rs`'s module docs).
//!
//! Synthetic `FeedFrame` unit tests cannot prove any of this: they inject
//! frames directly and never touch the relay's subscription registry, which
//! is exactly how the original bug stayed hidden. These tests drive the real
//! [`buzz_waker::relay_feed::RelayFeed`] transport — real NIP-42 handshake,
//! real REQ/CLOSE frames — against a real running relay, and assert on what
//! actually gets delivered.
//!
//! # Running
//!
//! Requires a running relay with `BUZZ_AUTO_MIGRATE=true` on first start and
//! `BUZZ_REQUIRE_AUTH_TOKEN` unset/false (dev-mode `X-Pubkey` auth):
//!
//! ```text
//! cargo test --test e2e_waker_channel_fanout -- --ignored
//! ```
//!
//! Override the relay URL with `RELAY_URL` (matches `e2e_relay.rs`).

use std::time::Duration;

use buzz_waker::cursor::{CursorStore, DEFAULT_COMPLETED_RING};
use buzz_waker::decide::WAKE_TRIGGER_KINDS;
use buzz_waker::feed::{
    step, wake_live_subscription_id, FeedFrame, FeedStep, FeedTransport, WakeReplay,
    REPLAY_PAGE_LIMIT,
};
use buzz_waker::relay_feed::RelayFeed;
use nostr::{EventBuilder, Keys, Kind, Tag};
use uuid::Uuid;

fn relay_url() -> String {
    std::env::var("RELAY_URL").unwrap_or_else(|_| "ws://localhost:3000".to_string())
}

fn relay_http_url() -> String {
    relay_url()
        .replace("wss://", "https://")
        .replace("ws://", "http://")
        .trim_end_matches('/')
        .to_string()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_secs()
}

/// Submit a signed event over HTTP using the relay's dev-mode `X-Pubkey`
/// fallback — no NIP-98 signing needed, matching `e2e_relay.rs`'s
/// `create_test_channel`. Requires `BUZZ_REQUIRE_AUTH_TOKEN` unset/false.
async fn submit_event(keys: &Keys, event: &nostr::Event) {
    let resp = reqwest::Client::new()
        .post(format!("{}/events", relay_http_url()))
        .header("X-Pubkey", keys.public_key().to_hex())
        .header("Content-Type", "application/json")
        .body(serde_json::to_string(event).expect("serialize event"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("submit event failed: {e}"));
    assert!(
        resp.status().is_success(),
        "submit event rejected: {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.expect("parse response");
    assert!(
        body["accepted"].as_bool().unwrap_or(false),
        "event not accepted: {body}"
    );
}

/// Create a real channel via kind:9007, owned by `keys`.
async fn create_test_channel(keys: &Keys) -> Uuid {
    let channel_id = Uuid::new_v4();
    let event = EventBuilder::new(Kind::Custom(9007), "")
        .tags(vec![
            Tag::parse(["h", &channel_id.to_string()]).unwrap(),
            Tag::parse(["name", &format!("waker-e2e-{channel_id}")]).unwrap(),
            Tag::parse(["channel_type", "stream"]).unwrap(),
            Tag::parse(["visibility", "open"]).unwrap(),
        ])
        .sign_with_keys(keys)
        .expect("sign kind:9007");
    submit_event(keys, &event).await;
    channel_id
}

/// Add `member_pubkey_hex` to `channel_id` via kind:9000 (PUT_USER), signed
/// by `owner_keys`.
async fn add_channel_member(owner_keys: &Keys, channel_id: Uuid, member_pubkey_hex: &str) {
    let event = EventBuilder::new(Kind::Custom(9000), "")
        .tags(vec![
            Tag::parse(["h", &channel_id.to_string()]).unwrap(),
            Tag::parse(["p", member_pubkey_hex]).unwrap(),
        ])
        .sign_with_keys(owner_keys)
        .expect("sign kind:9000");
    submit_event(owner_keys, &event).await;
}

/// Remove `member_pubkey_hex` from `channel_id` via kind:9001 (REMOVE_USER),
/// signed by `owner_keys`.
async fn remove_channel_member(owner_keys: &Keys, channel_id: Uuid, member_pubkey_hex: &str) {
    let event = EventBuilder::new(Kind::Custom(9001), "")
        .tags(vec![
            Tag::parse(["h", &channel_id.to_string()]).unwrap(),
            Tag::parse(["p", member_pubkey_hex]).unwrap(),
        ])
        .sign_with_keys(owner_keys)
        .expect("sign kind:9001");
    submit_event(owner_keys, &event).await;
}

/// Build (unsigned) a wake-trigger mention: kind:40002, `#h` = channel,
/// `#p` = agent, content = `label` (so a test can recognise it back out).
fn mention_event(
    author_keys: &Keys,
    channel_id: Uuid,
    agent_pubkey_hex: &str,
    label: &str,
) -> nostr::Event {
    EventBuilder::new(Kind::Custom(WAKE_TRIGGER_KINDS[1] as u16), label)
        .tags(vec![
            Tag::parse(["h", &channel_id.to_string()]).unwrap(),
            Tag::parse(["p", agent_pubkey_hex]).unwrap(),
        ])
        .sign_with_keys(author_keys)
        .expect("sign mention event")
}

/// Same as [`mention_event`], but with an explicit `created_at` instead of
/// whatever second the signing call happens to land in.
///
/// Needed for bulk filler history: events signed back-to-back in a tight
/// loop can land on the same wall-clock second depending on machine speed,
/// and if enough of them tie at exactly the page boundary,
/// `WakeReplay::on_eose`'s "stalled on a tied second" guard reports the walk
/// truncated instead of paging — a flake that has nothing to do with what
/// the test is actually checking.
fn mention_event_at(
    author_keys: &Keys,
    channel_id: Uuid,
    agent_pubkey_hex: &str,
    label: &str,
    created_at: u64,
) -> nostr::Event {
    EventBuilder::new(Kind::Custom(WAKE_TRIGGER_KINDS[1] as u16), label)
        .tags(vec![
            Tag::parse(["h", &channel_id.to_string()]).unwrap(),
            Tag::parse(["p", agent_pubkey_hex]).unwrap(),
        ])
        .custom_created_at(nostr::Timestamp::from(created_at))
        .sign_with_keys(author_keys)
        .expect("sign mention event")
}

/// Compact one-line summary of a step batch — a bulk-history test can collect
/// well over a thousand `Admitted` steps, and dumping the full step list
/// into a panic message is unreadable at that size.
fn summarize(steps: &[(FeedFrame, FeedStep)]) -> String {
    let admitted = steps
        .iter()
        .filter(|(_, s)| matches!(s, FeedStep::Admitted { .. }))
        .count();
    let non_admitted: Vec<&FeedStep> = steps
        .iter()
        .filter(|(_, s)| !matches!(s, FeedStep::Admitted { .. }))
        .map(|(_, s)| s)
        .collect();
    format!(
        "{} steps total ({admitted} Admitted, not shown); non-Admitted: {non_admitted:?}",
        steps.len()
    )
}

/// Drive `feed`'s frames through `step`, collecting every (raw frame, fold
/// outcome) pair, until `stop` returns `true` for one outcome or `budget`
/// elapses.
///
/// Returns the raw [`FeedFrame`] alongside each [`FeedStep`] — `FeedStep`
/// alone does not name the subscription an admitted trigger arrived on, and
/// a test proving delivery came specifically via a live subscription (rather
/// than, say, racing with backfill's own historical query) needs that.
///
/// Panics on a transport error or timeout — both are real test failures
/// here, never a quiet "maybe it just didn't happen yet".
async fn drain_until(
    feed: &mut RelayFeed,
    cursor: &mut CursorStore,
    replay: &mut WakeReplay,
    budget: Duration,
    mut stop: impl FnMut(&FeedStep) -> bool,
) -> Vec<(FeedFrame, FeedStep)> {
    let mut steps = Vec::new();
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!(
                "drain_until timed out after {budget:?} waiting for the expected step; saw: {}",
                summarize(&steps)
            );
        }
        let frame = match feed.next_frame(remaining.as_secs().max(1)).await {
            Ok(Some(frame)) => frame,
            Ok(None) => continue,
            Err(e) => panic!("transport error while draining: {e}"),
        };
        let outcome = step(cursor, replay, &frame, now_secs()).expect("cursor step");
        // Standing in for the daemon that doesn't exist yet (a separate PR —
        // see feed.rs's module docs): a clamped page means the walk isn't
        // done, and `FeedStep::Backfill` is the caller's cue to issue the
        // next page. Skipping this would strand the walk after page one on
        // any backlog over `REPLAY_PAGE_LIMIT` rows. Awaiting this before
        // `stop` can return also gives callers a happens-before guarantee:
        // once this call returns having observed a `Backfill` step, the next
        // page's REQ — with its `until` bound already fixed below whatever
        // "now" was when page one ran — has already been sent.
        if let FeedStep::Backfill { since, until } = &outcome {
            feed.subscribe_backfill(*since, Some(*until))
                .await
                .expect("subscribe next backfill page");
        }
        let done = stop(&outcome);
        steps.push((frame, outcome));
        if done {
            return steps;
        }
    }
}

async fn setup_cursor() -> (tempfile::TempDir, CursorStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cursor.json");
    let now = now_secs();
    let cursor =
        CursorStore::open_or_start(path, now, DEFAULT_COMPLETED_RING).expect("open cursor");
    (dir, cursor)
}

fn resume_since(cursor: &mut CursorStore) -> u64 {
    match cursor.resume(now_secs()) {
        buzz_waker::Resume::Since(s) => s,
        buzz_waker::Resume::GapTooOld { since, .. } => since,
    }
}

/// The core finding this whole PR responds to: a live subscription for a
/// channel the agent belongs to actually receives that channel's fan-out.
/// Before this PR's fix, an agent's live subscription was `#p`-only with no
/// `#h`, and the relay's `fan_out_scoped` structurally excludes a
/// channel-less subscription from ever matching a channel-scoped event — so
/// this exact scenario delivered nothing, silently, forever.
#[tokio::test]
#[ignore]
async fn a_mention_published_after_the_channel_live_subscription_opens_is_delivered() {
    let url = relay_url();
    let owner_keys = Keys::generate();
    let agent_keys = Keys::generate();
    let agent_pubkey = agent_keys.public_key().to_hex();

    let channel_id = create_test_channel(&owner_keys).await;
    add_channel_member(&owner_keys, channel_id, &agent_pubkey).await;

    let (_dir, mut cursor) = setup_cursor().await;
    let since = resume_since(&mut cursor);
    let mut replay = WakeReplay::new(since);

    let mut feed = RelayFeed::new(url, agent_keys, None);
    feed.connect()
        .await
        .expect("agent connects and authenticates");
    let discovered = feed.discover_channels().await.expect("discover_channels");
    assert!(
        discovered.contains(&channel_id),
        "the agent must discover the channel it was just added to, got {discovered:?}"
    );
    feed.subscribe_membership(since)
        .await
        .expect("subscribe membership");
    feed.subscribe_channel_live(channel_id, since)
        .await
        .expect("subscribe channel live");
    feed.subscribe_backfill(since, None)
        .await
        .expect("subscribe backfill");

    // Drain the (empty) backlog first, so what follows is unambiguously live.
    drain_until(
        &mut feed,
        &mut cursor,
        &mut replay,
        Duration::from_secs(15),
        |s| matches!(s, FeedStep::ReplayComplete { .. }),
    )
    .await;
    feed.close_backfill().await.expect("close backfill");

    let mention = mention_event(&owner_keys, channel_id, &agent_pubkey, "live-fanout-probe");
    let mention_id = mention.id.to_hex();
    submit_event(&owner_keys, &mention).await;

    let steps = drain_until(
        &mut feed,
        &mut cursor,
        &mut replay,
        Duration::from_secs(15),
        |s| matches!(s, FeedStep::Admitted { event, .. } if event.id == mention_id),
    )
    .await;

    assert!(
        steps
            .iter()
            .any(|(_, s)| matches!(s, FeedStep::Admitted { event, .. } if event.id == mention_id)),
        "the mention published after the live subscription opened must be \
         admitted — {}",
        summarize(&steps)
    );
}

/// Alex's named race: a mention published while the backfill walk is still
/// paging through a real backlog must still be delivered — via the live
/// subscription, since its `created_at` is newer than backfill will ever
/// page down to (backfill pages strictly backward from `since`/`until`
/// bounds all older than "now").
///
/// Publishes just over one page of filler history so the walk genuinely
/// takes more than one REQ. Two things this test got wrong on first review
/// (Alex, on `7ce0f88a9`) and now guards against:
///
/// - Sending backfill's first (unbounded) REQ over the WebSocket and then
///   publishing the probe over a *separate* HTTP connection does not order
///   the relay's DB query before the insert. If the relay happens to run
///   that query after the probe lands, the unbounded first page legitimately
///   contains the probe — the test would then pass even with live fan-out
///   completely broken, because `FeedStep::Admitted` alone cannot say which
///   subscription delivered it. Fixed by publishing the probe only *after*
///   `drain_until` has already observed `FeedStep::Backfill` for page one:
///   that step means the second page's REQ — carrying an `until` bound fixed
///   below whatever "now" was when page one ran — has already been sent, so
///   nothing published from this point on can ever appear in a later page,
///   full stop, not just probably. And separately, this test now asserts the
///   raw frame's subscription id for the probe's admission equals
///   `wake_live_subscription_id(probe_channel)` directly, rather than
///   inferring the source from timing alone.
/// - Filler events signed back-to-back in a tight loop can tie to the same
///   wall-clock second depending on machine speed; enough ties exactly at
///   the page boundary make `WakeReplay::on_eose`'s "stalled on a tied
///   second" guard report `truncated` instead of paging. Fixed by giving
///   filler deterministic, one-second-apart `created_at` values instead of
///   relying on signing-time `now()`.
#[tokio::test]
#[ignore]
async fn a_mention_published_during_an_active_backfill_walk_is_still_delivered_live() {
    let url = relay_url();
    let owner_keys = Keys::generate();
    let agent_keys = Keys::generate();
    let agent_pubkey = agent_keys.public_key().to_hex();

    // Two channels: `backlog_channel` carries the filler history and gets no
    // live subscription, so it is drained only by backfill's own paging —
    // `probe_channel` gets the live subscription and carries the probe. This
    // keeps total frame volume to roughly one copy of the backlog rather
    // than two: the live subscription shares backfill's floor (see feed.rs's
    // module docs) and would otherwise independently redeliver every filler
    // event too, doubling an already-large drain for no additional coverage.
    let backlog_channel = create_test_channel(&owner_keys).await;
    let probe_channel = create_test_channel(&owner_keys).await;
    add_channel_member(&owner_keys, backlog_channel, &agent_pubkey).await;
    add_channel_member(&owner_keys, probe_channel, &agent_pubkey).await;

    // Just over one page, published and settled before the agent ever
    // connects — enough to force backfill past its first page. Deterministic,
    // strictly increasing timestamps one second apart: see the "tied second"
    // note above.
    let filler_count = REPLAY_PAGE_LIMIT + 20;
    let filler_base = now_secs().saturating_sub(u64::from(filler_count) + 60);
    let mut filler = Vec::with_capacity(filler_count as usize);
    for i in 0..filler_count {
        filler.push(mention_event_at(
            &owner_keys,
            backlog_channel,
            &agent_pubkey,
            &format!("filler-{i}"),
            filler_base + u64::from(i),
        ));
    }
    // Bounded concurrency so this does not serialize into a multi-minute
    // setup step, but does not overwhelm the relay's connection budget either.
    use futures_util::stream::{self, StreamExt};
    stream::iter(filler.iter())
        .for_each_concurrent(16, |event| submit_event(&owner_keys, event))
        .await;

    let (_dir, mut cursor) = setup_cursor().await;
    let since = resume_since(&mut cursor);
    let mut replay = WakeReplay::new(since);

    let mut feed = RelayFeed::new(url, agent_keys, None);
    feed.connect()
        .await
        .expect("agent connects and authenticates");
    feed.discover_channels().await.expect("discover_channels");
    feed.subscribe_membership(since)
        .await
        .expect("subscribe membership");
    // No live subscription for backlog_channel — deliberately, see above.
    feed.subscribe_channel_live(probe_channel, since)
        .await
        .expect("subscribe channel live for the probe channel");
    feed.subscribe_backfill(since, None)
        .await
        .expect("subscribe backfill — page one, unbounded");

    // Wait for page one's clamp to be diagnosed. `drain_until` reacts to
    // `FeedStep::Backfill` by sending the second page's REQ *before*
    // returning, so by the time this call is done, that bounded `until` has
    // already gone out to the relay — a real happens-before, not a timing
    // hope.
    let first_page_steps = drain_until(
        &mut feed,
        &mut cursor,
        &mut replay,
        Duration::from_secs(30),
        |s| matches!(s, FeedStep::Backfill { .. }),
    )
    .await;
    assert!(
        first_page_steps
            .iter()
            .any(|(_, s)| matches!(s, FeedStep::Backfill { .. })),
        "filler history must be large enough to force a second backfill page, \
         or this test proves nothing about an *active* walk — {}",
        summarize(&first_page_steps)
    );

    // Safe now: the probe's created_at (now) is newer than every backfill
    // page's bound already committed to a REQ, so it can never appear in a
    // later page — only probe_channel's live subscription can deliver it.
    let probe = mention_event(&owner_keys, probe_channel, &agent_pubkey, "mid-walk-probe");
    let probe_id = probe.id.to_hex();
    submit_event(&owner_keys, &probe).await;

    // Stop on the probe's own admission specifically, not on
    // `ReplayComplete`: the probe arrives via a delivery path independent of
    // backfill's page-2 pipeline, and the two frames can interleave in
    // either order. Waiting for `ReplayComplete` risked the drain ending
    // right as page two finished but before the live-delivered probe frame
    // had arrived — a false failure with nothing wrong underneath.
    let rest_steps = drain_until(
        &mut feed,
        &mut cursor,
        &mut replay,
        Duration::from_secs(60),
        |s| matches!(s, FeedStep::Admitted { event, .. } if event.id == probe_id),
    )
    .await;

    let (probe_frame, _) = rest_steps
        .iter()
        .find(|(_, s)| matches!(s, FeedStep::Admitted { event, .. } if event.id == probe_id))
        .unwrap_or_else(|| {
            panic!(
                "a mention newer than every backfill page's bound can only arrive via \
                 the live subscription — it must be admitted despite the walk still \
                 running when it was published. {}",
                summarize(&rest_steps)
            )
        });
    let probe_subscription_id = match probe_frame {
        FeedFrame::Event {
            subscription_id, ..
        } => subscription_id.as_str(),
        other => panic!("expected an Event frame for the admitted probe, got {other:?}"),
    };
    assert_eq!(
        probe_subscription_id,
        wake_live_subscription_id(probe_channel),
        "the probe must be delivered specifically on probe_channel's live \
         subscription, not merely admitted from some subscription — the \
         backfill walk (still running when the probe was published) must \
         never be able to produce it"
    );
}

/// A membership add must open the channel's live subscription, and a
/// membership remove must close it — proven against the real relay, because
/// only the real relay can show the CLOSE actually stopped delivery (a mock
/// could trivially "forget" to enforce it and this test would not catch it).
#[tokio::test]
#[ignore]
async fn membership_add_and_remove_open_and_close_the_live_subscription() {
    let url = relay_url();
    let owner_keys = Keys::generate();
    let agent_keys = Keys::generate();
    let agent_pubkey = agent_keys.public_key().to_hex();

    let channel_id = create_test_channel(&owner_keys).await;
    // Deliberately NOT added as a member yet.

    let (_dir, mut cursor) = setup_cursor().await;
    let since = resume_since(&mut cursor);
    let mut replay = WakeReplay::new(since);

    let mut feed = RelayFeed::new(url, agent_keys, None);
    feed.connect()
        .await
        .expect("agent connects and authenticates");
    let discovered = feed.discover_channels().await.expect("discover_channels");
    assert!(
        !discovered.contains(&channel_id),
        "the agent is not yet a member and must not discover this channel"
    );
    feed.subscribe_membership(since)
        .await
        .expect("subscribe membership");

    // --- Add: the membership watch must report it, and subscribing live
    // afterward must receive fan-out.
    add_channel_member(&owner_keys, channel_id, &agent_pubkey).await;
    let steps = drain_until(&mut feed, &mut cursor, &mut replay, Duration::from_secs(15), |s| {
        matches!(s, FeedStep::ChannelMembershipChanged { channel_id: c, added: true } if *c == channel_id)
    })
    .await;
    assert!(
        steps.iter().any(|(_, s)| matches!(
            s,
            FeedStep::ChannelMembershipChanged { channel_id: c, added: true } if *c == channel_id
        )),
        "adding the agent must surface ChannelMembershipChanged{{added:true}} \
         for this channel — {}",
        summarize(&steps)
    );

    feed.subscribe_channel_live(channel_id, since)
        .await
        .expect("subscribe channel live after the add notification");

    let present = mention_event(
        &owner_keys,
        channel_id,
        &agent_pubkey,
        "delivered-while-member",
    );
    let present_id = present.id.to_hex();
    submit_event(&owner_keys, &present).await;
    let steps = drain_until(
        &mut feed,
        &mut cursor,
        &mut replay,
        Duration::from_secs(15),
        |s| matches!(s, FeedStep::Admitted { event, .. } if event.id == present_id),
    )
    .await;
    assert!(
        steps
            .iter()
            .any(|(_, s)| matches!(s, FeedStep::Admitted { event, .. } if event.id == present_id)),
        "a mention published while the agent is a member with a live \
         subscription open must be admitted — {}",
        summarize(&steps)
    );

    // --- Remove: the membership watch must report it, and unsubscribing
    // live afterward must actually stop delivery — the part only a real
    // relay proves.
    remove_channel_member(&owner_keys, channel_id, &agent_pubkey).await;
    let steps = drain_until(&mut feed, &mut cursor, &mut replay, Duration::from_secs(15), |s| {
        matches!(s, FeedStep::ChannelMembershipChanged { channel_id: c, added: false } if *c == channel_id)
    })
    .await;
    assert!(
        steps.iter().any(|(_, s)| matches!(
            s,
            FeedStep::ChannelMembershipChanged { channel_id: c, added: false } if *c == channel_id
        )),
        "removing the agent must surface ChannelMembershipChanged{{added:false}} \
         for this channel — {}",
        summarize(&steps)
    );

    feed.unsubscribe_channel_live(channel_id)
        .await
        .expect("unsubscribe channel live after the remove notification");

    // A member-only channel: once removed, the owner is the sole remaining
    // member and the agent's subscription is gone, so a further mention
    // must never reach it. Cross-check with a second, still-open channel
    // subscription-free control: absence of a frame for the timeout window
    // is the evidence here, so the budget must be generous enough that a
    // real bug (subscription still open) would reliably show up instead of
    // racing a short timeout.
    let absent = mention_event(
        &owner_keys,
        channel_id,
        &agent_pubkey,
        "must-not-arrive-after-removal",
    );
    let absent_id = absent.id.to_hex();
    submit_event(&owner_keys, &absent).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match feed.next_frame(remaining.as_secs().max(1)).await {
            Ok(Some(frame)) => {
                let outcome =
                    step(&mut cursor, &mut replay, &frame, now_secs()).expect("cursor step");
                assert!(
                    !matches!(outcome, FeedStep::Admitted { ref event, .. } if event.id == absent_id),
                    "a mention published after unsubscribe_channel_live must never \
                     arrive — the CLOSE did not actually take effect on the relay"
                );
            }
            Ok(None) => continue,
            Err(e) => panic!("transport error while confirming absence: {e}"),
        }
    }
}
