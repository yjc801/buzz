//! Connection setup must not outlast a cancel, or a relay that says nothing.
//!
//! Two waits here had no bound. On 2026-11-09 at 23:50:03Z a wake loop was
//! cancelled 15s after it started, while pacing the subscriptions of 106
//! channels; setup did not look at the cancel token, so the loop kept the
//! agent's cursor until about 23:50:19Z and the agent's next wake loop could
//! not start before then. And nothing bounded the connect itself: a relay
//! that accepts the socket and never answers the upgrade held a tap, or a
//! wake loop's first connection, for good and without a log line.
//!
//! Both are driven through the real [`run_wake_loop`] and the real taps
//! against a relay double on a real socket.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::*;
use crate::bundle_feed::{run_bundle_tap, BundleState};
use crate::credential_feed::{run_credential_tap, CredentialState};
use crate::floors::FloorStore;
use crate::presence_feed::{run_presence_tap, PresenceState};
use crate::roster_feed::{run_roster_tap, Admission, RosterState};

/// Channels the relay double discovers: one burst, then 18s of pacing.
const CHANNELS: usize = SUBSCRIBE_BURST + 60;

/// The setup step a [`Relay`] leaves the wake loop waiting in.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// The socket is accepted and the upgrade never answered.
    Connect,
    /// AUTH completes and channel discovery is never answered.
    Discovery,
    /// Discovery is answered and the archived-channel probe never is.
    ArchiveProbe,
    /// Everything is answered; the loop is spacing out its subscriptions.
    Pacing,
}

/// What a [`Relay`] has received, across all its connections.
#[derive(Default)]
struct Seen {
    connections: AtomicUsize,
    discoveries: AtomicUsize,
    archive_probes: AtomicUsize,
    live_subscriptions: AtomicUsize,
}

/// A relay double that answers up to `step` and no further, on every
/// connection.
struct Relay {
    url: String,
    seen: Arc<Seen>,
}

impl Relay {
    async fn stopping_at(step: Step) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Seen::default());
        let relay_seen = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                seen.connections.fetch_add(1, Ordering::SeqCst);
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    if matches!(step, Step::Connect) {
                        // Held open and never read: the client's upgrade
                        // request goes unanswered.
                        let _held = stream;
                        return std::future::pending().await;
                    }
                    serve(stream, step, &seen).await;
                });
            }
        });
        Self {
            url,
            seen: relay_seen,
        }
    }

    /// Whether the wake loop has got as far as `step`.
    fn reached(&self, step: Step) -> bool {
        let count = match step {
            Step::Connect => &self.seen.connections,
            Step::Discovery => &self.seen.discoveries,
            Step::ArchiveProbe => &self.seen.archive_probes,
            // Past the burst: the loop is sleeping between subscriptions.
            Step::Pacing => return self.live_subscriptions() > SUBSCRIBE_BURST,
        };
        count.load(Ordering::SeqCst) > 0
    }

    fn live_subscriptions(&self) -> usize {
        self.seen.live_subscriptions.load(Ordering::SeqCst)
    }
}

/// NIP-42, then a bare EOSE for every REQ — after [`CHANNELS`] memberships
/// for discovery — except the one `step` stops at, which is never answered.
async fn serve(stream: tokio::net::TcpStream, step: Step, seen: &Seen) {
    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let signer = Keys::generate();
    let text = |v: serde_json::Value| WsMessage::Text(v.to_string().into());
    let _ = ws.send(text(serde_json::json!(["AUTH", "c"]))).await;
    while let Some(Ok(WsMessage::Text(frame))) = ws.next().await {
        let Ok(frame) = serde_json::from_str::<Vec<serde_json::Value>>(&frame) else {
            continue;
        };
        match frame[0].as_str() {
            Some("AUTH") => {
                let ok = serde_json::json!(["OK", frame[1]["id"], true, ""]);
                let _ = ws.send(text(ok)).await;
            }
            Some("REQ") => {
                let sub = frame[1].as_str().unwrap_or_default().to_string();
                let discovery = frame[2]["kinds"].as_array().is_some_and(|kinds| {
                    kinds.contains(&serde_json::json!(
                        buzz_core::kind::KIND_NIP29_GROUP_MEMBERS
                    ))
                });
                if discovery {
                    seen.discoveries.fetch_add(1, Ordering::SeqCst);
                    if matches!(step, Step::Discovery) {
                        continue;
                    }
                    for channel in 1..=CHANNELS as u128 {
                        let d = Uuid::from_u128(channel).to_string();
                        let event = nostr::EventBuilder::new(
                            nostr::Kind::Custom(buzz_core::kind::KIND_NIP29_GROUP_MEMBERS as u16),
                            "",
                        )
                        .tags([nostr::Tag::parse(["d", &d]).unwrap()])
                        .sign_with_keys(&signer)
                        .unwrap();
                        let _ = ws
                            .send(text(serde_json::json!(["EVENT", sub, event])))
                            .await;
                    }
                } else if sub == "buzz-waker-archived" {
                    seen.archive_probes.fetch_add(1, Ordering::SeqCst);
                    if matches!(step, Step::ArchiveProbe) {
                        continue;
                    }
                } else if (1..=CHANNELS as u128)
                    .any(|c| sub == crate::feed::wake_live_subscription_id(Uuid::from_u128(c)))
                {
                    seen.live_subscriptions.fetch_add(1, Ordering::SeqCst);
                }
                let _ = ws.send(text(serde_json::json!(["EOSE", sub]))).await;
            }
            _ => {}
        }
    }
}

fn config(relay: &Relay, cursor_path: PathBuf) -> WakeLoopConfig {
    WakeLoopConfig {
        relay_url: relay.url.clone(),
        keys: Keys::generate(),
        auth_tag: None,
        cursor_path,
        presence_state: Arc::new(PresenceState::new()),
        watch_list: crate::watch_list::WatchList::new(),
        bundle_state: Arc::new(BundleState::new()),
        provider_env: None,
        channel_refresh: CHANNEL_REFRESH_INTERVAL,
        refresh_setup_deadline: REFRESH_SETUP_DEADLINE,
    }
}

/// Everything logged on this thread while the guard is held.
#[derive(Clone, Default)]
struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        let writer = self.clone();
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .finish(),
        )
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// A wake loop cancelled anywhere in setup returns at once, with the cursor
/// free for the next one, and without reporting the step it was in as failed.
///
/// Before, each row ran its step out first: the connect for as long as the
/// relay held the socket, discovery and the archive probe to their 20s
/// timeouts, pacing for the 18s these channels take.
#[tokio::test]
async fn a_wake_loop_cancelled_during_setup_returns_at_once() {
    for step in [
        Step::Connect,
        Step::Discovery,
        Step::ArchiveProbe,
        Step::Pacing,
    ] {
        let logs = Logs::default();
        let _capture = logs.capture();
        let relay = Relay::stopping_at(step).await;
        let dir = tempfile::tempdir().unwrap();
        let cursor_path = dir.path().join("cursor.json");
        let (_start_tx, start_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_wake_loop(
            config(&relay, cursor_path.clone()),
            start_rx,
            cancel.clone(),
        ));

        tokio::time::timeout(Duration::from_secs(10), async {
            while !relay.reached(step) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{step:?}: the wake loop never got that far"));
        assert!(!task.is_finished(), "{step:?}: the wake loop is in setup");

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap_or_else(|_| panic!("{step:?}: a cancelled setup ran on"))
            .expect("the wake loop does not panic");

        CursorStore::open_or_start(cursor_path, now_secs(), DEFAULT_COMPLETED_RING)
            .unwrap_or_else(|error| panic!("{step:?}: the cursor is still held: {error}"));
        let logged = logs.text();
        assert!(
            !logged.contains("WARN") && !logged.contains("ERROR"),
            "{step:?}: a cancel is not a failed step:\n{logged}"
        );
    }
}

/// Whether `connection` — a wake loop or a tap, run here to completion or
/// until this returns — dials `relay` a second time.
async fn redials(relay: &Relay, connection: impl Future<Output = ()>) -> bool {
    let redialled = async {
        // Paused time: an hour of it passes as fast as these sleeps can run.
        for _ in 0..36_000 {
            if relay.seen.connections.load(Ordering::SeqCst) > 1 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    };
    tokio::select! {
        () = connection => false,
        redialled = redialled => redialled,
    }
}

/// Every connection this daemon opens gives up on a relay that accepts the
/// socket and never answers the upgrade, and takes its usual failure path:
/// it backs off and dials again.
///
/// Before, each of them waited on that first socket for as long as the relay
/// kept it open. For a credential tap that is an enrolled agent that is never
/// started; for a wake loop's first connection, one that holds the agent's
/// cursor and watches nothing.
#[tokio::test(start_paused = true)]
async fn every_connection_gives_up_on_a_relay_that_never_answers_the_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let keys = Keys::generate();
    let owner = Keys::generate().public_key().to_hex();
    let agent = Keys::generate().public_key().to_hex();
    let cancel = CancellationToken::new();
    let floor = |name: &str| FloorStore::enroll(dir.path().join(name), &owner).expect("enroll");

    let relay = Relay::stopping_at(Step::Connect).await;
    let (_start_tx, start_rx) = mpsc::channel(1);
    let wake_loop = run_wake_loop(
        config(&relay, dir.path().join("cursor.json")),
        start_rx,
        cancel.clone(),
    );
    assert!(redials(&relay, wake_loop).await, "wake loop");

    let relay = Relay::stopping_at(Step::Connect).await;
    let presence = PresenceState::new();
    let tap = run_presence_tap(&relay.url, &keys, None, &presence, &cancel);
    assert!(redials(&relay, tap).await, "presence tap");

    let relay = Relay::stopping_at(Step::Connect).await;
    let (bundles, mut bundle_floor) = (BundleState::new(), floor("floor.json"));
    let (start_tx, _start_rx) = mpsc::channel(1);
    let tap = run_bundle_tap(
        &relay.url,
        &keys,
        None,
        &owner,
        &mut bundle_floor,
        &bundles,
        &start_tx,
        &cancel,
    );
    assert!(redials(&relay, tap).await, "bundle tap");

    let relay = Relay::stopping_at(Step::Connect).await;
    let (credentials, mut credential_floor) =
        (CredentialState::new(), floor("credential_floor.json"));
    let tap = run_credential_tap(
        &relay.url,
        &keys,
        None,
        &owner,
        &agent,
        &mut credential_floor,
        &credentials,
        &cancel,
    );
    assert!(redials(&relay, tap).await, "credential tap");

    let relay = Relay::stopping_at(Step::Connect).await;
    let (admission, roster) = (Admission::Closed(vec![owner.clone()]), RosterState::new());
    let tap = run_roster_tap(
        &relay.url,
        &keys,
        None,
        &admission,
        dir.path(),
        &roster,
        &cancel,
    );
    assert!(redials(&relay, tap).await, "roster tap");
}
