//! A restarted daemon must get every enrolled agent's credential back.
//!
//! On 2026-11-09 the daemon restarted and three of six agents were never
//! started again: each was adopted from the roster, its credential tap
//! connected, and the relay served that tap nothing. The relay cuts an
//! envelope query to `limit` rows *before* it matches `#d`, so the tap's
//! `limit: 16` is the owner's sixteen newest envelopes to this waker — for
//! every agent and the roster together — and an agent whose credential is
//! older than those has none served at all.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use nostr::{EventBuilder, Kind, Timestamp, ToBech32};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::*;
use crate::roster_feed::ROSTER_D_TAG;

/// The relay's row cap (`buzz_db::DEFAULT_MAX_PAGE_LIMIT`).
const RELAY_ROW_CAP: usize = 1_000;

fn tag_values(filter: &Value, key: &str) -> Option<Vec<String>> {
    let values = filter.get(key)?.as_array()?;
    Some(
        values
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect(),
    )
}

fn has_tag(event: &nostr::Event, name: &str, values: &[String]) -> bool {
    event.tags.iter().any(|tag| {
        let parts = tag.as_slice();
        parts.len() >= 2 && parts[0] == name && values.contains(&parts[1])
    })
}

/// Answer one envelope filter the way `buzz-relay` does.
///
/// `kinds`, `authors`, a single `#p`, `since` and the `until`/`before_id`
/// cursor select the rows; they are ordered `created_at DESC, id ASC` and cut
/// to `limit`; and only then is `#d` matched. The envelope kind is not
/// NIP-33, so `#d` never reaches the relay's query
/// (`buzz-relay/src/handlers/req.rs`, `filter_to_query_params`) and is left to
/// the per-event filter that runs on the page the query returned.
fn answer<'a>(stored: &'a [nostr::Event], filter: &Value) -> Vec<&'a nostr::Event> {
    let authors = tag_values(filter, "authors");
    let recipients = tag_values(filter, "#p");
    let coordinates = tag_values(filter, "#d");
    let since = filter["since"].as_u64();
    let until = filter["until"].as_u64();
    let before_id = filter["before_id"].as_str();
    let limit = filter["limit"]
        .as_u64()
        .map_or(RELAY_ROW_CAP, |limit| (limit as usize).min(RELAY_ROW_CAP));

    let mut rows: Vec<&nostr::Event> = stored
        .iter()
        .filter(|event| {
            let created_at = event.created_at.as_secs();
            let id = event.id.to_hex();
            authors
                .as_ref()
                .is_none_or(|authors| authors.contains(&event.pubkey.to_hex()))
                && recipients
                    .as_ref()
                    .is_none_or(|recipients| has_tag(event, "p", recipients))
                && since.is_none_or(|since| created_at >= since)
                && match (until, before_id) {
                    (Some(until), Some(before_id)) => {
                        created_at < until || (created_at == until && id.as_str() > before_id)
                    }
                    (Some(until), None) => created_at <= until,
                    (None, _) => true,
                }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.to_hex().cmp(&b.id.to_hex()))
    });
    rows.truncate(limit);
    rows.retain(|event| {
        coordinates
            .as_ref()
            .is_none_or(|coordinates| has_tag(event, "d", coordinates))
    });
    rows
}

/// A relay double serving `stored` through [`answer`], to any number of
/// connections, acknowledging every CLOSE with an empty-reason CLOSED as the
/// relay does.
struct Relay {
    url: String,
    /// Every REQ and CLOSE received, with the connection it arrived on.
    frames: Arc<Mutex<Vec<(usize, Value)>>>,
    /// Bumped to make every open connection hang up.
    hang_up: tokio::sync::watch::Sender<u32>,
    /// While set, a catch-up REQ is refused with a CLOSED instead of answered.
    refuse_catch_up: Arc<AtomicBool>,
}

impl Relay {
    async fn serving(stored: Vec<nostr::Event>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let url = format!("ws://{}", listener.local_addr().expect("local addr"));
        let frames = Arc::new(Mutex::new(Vec::new()));
        let (hang_up, _) = tokio::sync::watch::channel(0);
        let refuse_catch_up = Arc::new(AtomicBool::new(false));
        let stored = Arc::new(stored);

        let accepted = (
            Arc::clone(&frames),
            hang_up.clone(),
            Arc::clone(&refuse_catch_up),
        );
        tokio::spawn(async move {
            let (frames, hang_up, refuse_catch_up) = accepted;
            let mut connection = 0usize;
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(
                    stream,
                    connection,
                    Arc::clone(&stored),
                    Arc::clone(&frames),
                    hang_up.subscribe(),
                    Arc::clone(&refuse_catch_up),
                ));
                connection += 1;
            }
        });

        Self {
            url,
            frames,
            hang_up,
            refuse_catch_up,
        }
    }

    /// What `agent`'s tap sent, one list per connection, oldest first. A
    /// connection belongs to the agent its first REQ names in `#d`.
    fn sent_by(&self, agent: &Keys) -> Vec<Vec<Value>> {
        let agent = agent.public_key().to_hex();
        let frames = self.frames.lock().unwrap_or_else(PoisonError::into_inner);
        let mut connections: Vec<(usize, Vec<Value>)> = Vec::new();
        for (connection, frame) in frames.iter() {
            match connections.iter_mut().find(|(id, _)| id == connection) {
                Some((_, sent)) => sent.push(frame.clone()),
                None => connections.push((*connection, vec![frame.clone()])),
            }
        }
        connections
            .into_iter()
            .map(|(_, sent)| sent)
            .filter(|sent| sent[0][2]["#d"] == json!([agent]))
            .collect()
    }
}

async fn serve(
    stream: tokio::net::TcpStream,
    connection: usize,
    stored: Arc<Vec<nostr::Event>>,
    frames: Arc<Mutex<Vec<(usize, Value)>>>,
    mut hang_up: tokio::sync::watch::Receiver<u32>,
    refuse_catch_up: Arc<AtomicBool>,
) {
    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let send = |value: Value| WsMessage::Text(value.to_string().into());

    // NIP-42: challenge, the client's signed AUTH event, OK.
    if ws.send(send(json!(["AUTH", "challenge"]))).await.is_err() {
        return;
    }
    let auth_id = loop {
        let Some(Ok(WsMessage::Text(text))) = ws.next().await else {
            return;
        };
        let frame: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if frame[0] == "AUTH" {
            break frame[1]["id"].clone();
        }
    };
    if ws
        .send(send(json!(["OK", auth_id, true, ""])))
        .await
        .is_err()
    {
        return;
    }

    loop {
        let text = tokio::select! {
            _ = hang_up.changed() => return,
            frame = ws.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => text,
                Some(Ok(_)) => continue,
                _ => return,
            },
        };
        let frame: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let mut replies = Vec::new();
        if frame[0] == "CLOSE" {
            replies.push(json!(["CLOSED", frame[1], ""]));
        } else if frame[0] == "REQ"
            && frame[1] == CREDENTIAL_CATCH_UP_SUBSCRIPTION_ID
            && refuse_catch_up.load(Ordering::SeqCst)
        {
            replies.push(json!(["CLOSED", frame[1], "rate-limited: slow down"]));
        } else if frame[0] == "REQ" {
            for event in answer(&stored, &frame[2]) {
                replies.push(json!(["EVENT", frame[1], event]));
            }
            replies.push(json!(["EOSE", frame[1]]));
        } else {
            continue;
        }
        frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((connection, frame));
        for reply in replies {
            if ws.send(send(reply)).await.is_err() {
                return;
            }
        }
    }
}

/// An owner-signed envelope to `waker`, as the desktop publishes one.
fn envelope(
    owner: &Keys,
    waker: &Keys,
    d_tag: &str,
    content: &str,
    created_at: u64,
) -> nostr::Event {
    EventBuilder::new(Kind::Custom(KIND_WAKER_BUNDLE_ENVELOPE as u16), content)
        .tags([
            Tag::parse(["d", d_tag]).expect("d tag"),
            Tag::parse(["p", &waker.public_key().to_hex()]).expect("p tag"),
        ])
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(owner)
        .expect("sign")
}

/// `agent`'s credential at `credential_version`, encrypted to `waker`.
fn credential(
    owner: &Keys,
    waker: &Keys,
    agent: &Keys,
    credential_version: u64,
    created_at: u64,
) -> nostr::Event {
    let agent_pubkey = agent.public_key().to_hex();
    let body = CredentialBody {
        agent_pubkey: agent_pubkey.clone(),
        nsec: agent.secret_key().to_bech32().expect("nsec"),
        auth_tag: None,
        provider_credential: None,
        credential_version,
        issued_at: created_at,
        revoked: false,
    };
    let keypair = nostr::secp256k1::Keypair::from_secret_key(nostr::SECP256K1, owner.secret_key());
    let signed = SignedCredential::sign(&body, &keypair).expect("sign the credential");
    let ciphertext = nostr::nips::nip44::encrypt(
        owner.secret_key(),
        &waker.public_key(),
        serde_json::to_string(&signed).expect("serialize"),
        nostr::nips::nip44::Version::V2,
    )
    .expect("encrypt");
    envelope(owner, waker, &agent_pubkey, &ciphertext, created_at)
}

/// A roster republish: another envelope from the same owner to the same
/// waker, which no credential tap reads.
fn roster(owner: &Keys, waker: &Keys, created_at: u64) -> nostr::Event {
    envelope(owner, waker, ROSTER_D_TAG, "roster", created_at)
}

/// One agent's credential tap, as the daemon runs it after a restart.
struct Tap {
    state: Arc<CredentialState>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    _state_dir: tempfile::TempDir,
}

impl Tap {
    /// Start `agent`'s tap. `accepted_version` is the credential floor the
    /// previous run left on disk; the credential itself was only ever held in
    /// memory, so the tap starts with none.
    fn start(
        relay: &Relay,
        owner: &Keys,
        waker: &Keys,
        agent: &Keys,
        accepted_version: u64,
    ) -> Self {
        let owner_pubkey = owner.public_key().to_hex();
        let state_dir = tempfile::tempdir().expect("tempdir");
        let mut floor_store = FloorStore::enroll(
            state_dir.path().join("credential_floor.json"),
            &owner_pubkey,
        )
        .expect("enroll");
        floor_store.admit(accepted_version).expect("floor");

        let state = Arc::new(CredentialState::new());
        let cancel = CancellationToken::new();
        let task = {
            let relay_url = relay.url.clone();
            let waker = waker.clone();
            let agent_pubkey = agent.public_key().to_hex();
            let state = Arc::clone(&state);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                run_credential_tap(
                    &relay_url,
                    &waker,
                    None,
                    &owner_pubkey,
                    &agent_pubkey,
                    &mut floor_store,
                    &state,
                    &cancel,
                )
                .await;
            })
        };
        Self {
            state,
            cancel,
            task,
            _state_dir: state_dir,
        }
    }

    fn version(&self) -> Option<u64> {
        self.state.current().map(|body| body.credential_version)
    }

    async fn stop(self) {
        self.cancel.cancel();
        self.task.await.expect("the tap task must not panic");
    }
}

/// Wait for `check`, failing the test rather than hanging if it never holds.
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let waited = tokio::time::timeout(Duration::from_secs(10), async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "timed out waiting until {what}");
}

fn catch_up_requests(sent: &[Value]) -> Vec<&Value> {
    sent.iter()
        .filter(|frame| frame[0] == "REQ" && frame[1] == CREDENTIAL_CATCH_UP_SUBSCRIPTION_ID)
        .collect()
}

/// The incident. Three agents' credentials are older than the owner's sixteen
/// newest envelopes to the waker, because a fourth was reissued ten times
/// since, each with a roster republish. The daemon restarts: every tap starts
/// with nothing in memory and must get its agent's credential from the relay.
#[tokio::test]
async fn a_restarted_daemon_gets_every_enrolled_agents_credential() {
    let (owner, waker) = (Keys::generate(), Keys::generate());
    let (first, second, third, recent) = (
        Keys::generate(),
        Keys::generate(),
        Keys::generate(),
        Keys::generate(),
    );
    let mut stored = vec![
        credential(&owner, &waker, &first, 3, 1_000),
        credential(&owner, &waker, &second, 5, 1_001),
        credential(&owner, &waker, &third, 2, 1_002),
    ];
    for reissue in 0..10 {
        let at = 1_100 + 2 * reissue;
        stored.push(credential(&owner, &waker, &recent, reissue + 1, at));
        stored.push(roster(&owner, &waker, at + 1));
    }
    let relay = Relay::serving(stored).await;

    let taps = [
        (Tap::start(&relay, &owner, &waker, &first, 3), 3),
        (Tap::start(&relay, &owner, &waker, &second, 5), 5),
        (Tap::start(&relay, &owner, &waker, &third, 2), 2),
        (Tap::start(&relay, &owner, &waker, &recent, 10), 10),
    ];
    eventually("every agent's tap holds its current credential", || {
        taps.iter()
            .all(|(tap, version)| tap.version() == Some(*version))
    })
    .await;

    // The relay acknowledges the catch-up's CLOSE with a CLOSED. Long enough
    // for a tap that read that as a refusal to have reconnected.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let first_sent = relay.sent_by(&first);
    assert_eq!(first_sent.len(), 1, "the tap kept its connection");
    assert_eq!(
        catch_up_requests(&first_sent[0]).len(),
        1,
        "one page reaches a credential twenty envelopes back: {first_sent:?}"
    );
    assert_eq!(
        first_sent[0].last(),
        Some(&json!(["CLOSE", CREDENTIAL_CATCH_UP_SUBSCRIPTION_ID])),
        "a finished catch-up must not stay subscribed to every agent's envelopes"
    );
    let recent_sent = relay.sent_by(&recent);
    assert_eq!(
        recent_sent,
        vec![vec![credential_req(
            &owner.public_key().to_hex(),
            &waker.public_key().to_hex(),
            &recent.public_key().to_hex(),
        )]],
        "an agent whose credential the tap's own query serves costs no second request"
    );

    // The connection drops and the tap reconnects. It already holds the
    // credential, so it must only ask for what is newer than it.
    let _ = relay.hang_up.send(1);
    eventually(
        "the first agent's tap has caught up on a second connection",
        || {
            relay
                .sent_by(&first)
                .get(1)
                .is_some_and(|sent| sent.last().is_some_and(|frame| frame[0] == "CLOSE"))
        },
    )
    .await;
    let reconnected = &relay.sent_by(&first)[1];
    let pages = catch_up_requests(reconnected);
    assert_eq!(pages.len(), 1);
    assert_eq!(
        pages[0][2]["since"],
        json!(1_000),
        "a reconnect's catch-up starts at the credential already read: {pages:?}"
    );
    assert_eq!(taps[0].0.version(), Some(3));

    for (tap, _) in taps {
        tap.stop().await;
    }
}

/// A credential further back than one page is still found, across timestamps
/// that tie, and the search stops at it.
#[tokio::test]
async fn the_catch_up_pages_back_until_it_reaches_the_credential() {
    let (owner, waker, agent) = (Keys::generate(), Keys::generate(), Keys::generate());
    let mut stored = vec![credential(&owner, &waker, &agent, 4, 1_000)];
    // Three envelopes a second, so every page boundary falls inside a tie.
    for index in 0..450 {
        stored.push(roster(&owner, &waker, 2_000 + index / 3));
    }
    let relay = Relay::serving(stored).await;

    let tap = Tap::start(&relay, &owner, &waker, &agent, 4);
    eventually("the tap holds the credential", || tap.version() == Some(4)).await;
    eventually("the catch-up is closed", || {
        relay.sent_by(&agent)[0]
            .last()
            .is_some_and(|frame| frame[0] == "CLOSE")
    })
    .await;

    let sent = relay.sent_by(&agent);
    let pages = catch_up_requests(&sent[0]);
    assert_eq!(
        pages.len(),
        3,
        "451 envelopes are three pages of 200: {pages:?}"
    );
    assert!(pages[0][2].get("until").is_none());
    for page in &pages[1..] {
        assert!(
            page[2]["until"].is_u64() && page[2]["before_id"].is_string(),
            "a later page continues from the relay's composite cursor: {page}"
        );
    }

    tap.stop().await;
}

/// A catch-up the relay refuses mid-search is a failed connection, retried on
/// the ladder — not an acknowledgement, and not the end of the search.
#[tokio::test]
async fn a_refused_catch_up_reconnects_and_searches_again() {
    let (owner, waker, agent) = (Keys::generate(), Keys::generate(), Keys::generate());
    let mut stored = vec![credential(&owner, &waker, &agent, 1, 1_000)];
    for index in 0..20 {
        stored.push(roster(&owner, &waker, 2_000 + index));
    }
    let relay = Relay::serving(stored).await;
    relay.refuse_catch_up.store(true, Ordering::SeqCst);

    let tap = Tap::start(&relay, &owner, &waker, &agent, 1);
    eventually("the tap has reconnected after the refusal", || {
        relay.sent_by(&agent).len() >= 2
    })
    .await;
    assert_eq!(tap.version(), None);

    relay.refuse_catch_up.store(false, Ordering::SeqCst);
    eventually("the tap holds the credential", || tap.version() == Some(1)).await;

    tap.stop().await;
}

/// A relay that ignores the cursor serves the same full page forever. The
/// search must stop instead, and say it stopped short.
#[test]
fn a_full_page_that_does_not_move_the_cursor_ends_the_catch_up() {
    let (owner, waker) = (Keys::generate(), Keys::generate());
    let row = roster(&owner, &waker, 2_000);
    let mut catch_up = CatchUp::default();

    for _ in 0..CREDENTIAL_CATCH_UP_PAGE_LIMIT {
        catch_up.row(&row);
    }
    assert_eq!(catch_up.end_page(), PageEnd::More);
    for _ in 0..CREDENTIAL_CATCH_UP_PAGE_LIMIT {
        catch_up.row(&row);
    }
    assert_eq!(catch_up.end_page(), PageEnd::GaveUp);

    let mut replay = Replay::CatchingUp(catch_up);
    for _ in 0..CREDENTIAL_CATCH_UP_PAGE_LIMIT {
        if let Replay::CatchingUp(catch_up) = &mut replay {
            catch_up.row(&row);
        }
    }
    let advance = replay.advance(true, false, None, "owner", "waker");
    assert!(advance.done && advance.gave_up, "{advance:?}");
    assert!(matches!(replay, Replay::Done));
}
